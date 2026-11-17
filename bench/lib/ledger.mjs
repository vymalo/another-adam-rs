// The ledger: an append-only JSON-lines file in $BENCH_HOME, the one record of what happened.
//
// Records (one JSON object per line, fsynced before `append` returns):
//   {kind:'state', run, state, ...}   one per state change of a run
//   {kind:'call',  run, attempt, call, phase, ...}   the meter's: pending, settled, refused, failed
//   {kind:'setup', ...}   what the report shows about the setup (label, seed, versions)
// Nothing in a record is a request or response body, a key or an endpoint.
//
// `foldRecords` gives each run's current state and its spend. `resumeActions` says what to do with
// each run when the harness starts again (a pure function of the fold).
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';

export const STATES = ['Planned', 'Reserved', 'Preparing', 'Running', 'Collecting', 'Grading', 'Done', 'NotStarted', 'StoppedByCap', 'Interrupted', 'Failed_infra'];

/** The lifecycle of a run (README.md, the state diagram). A run starts in Planned. */
export const TRANSITIONS = {
  Planned: ['Reserved', 'NotStarted'],
  Reserved: ['Preparing', 'Interrupted'],
  Preparing: ['Running', 'Failed_infra', 'Interrupted'],
  Running: ['Collecting', 'StoppedByCap', 'Interrupted'],
  Collecting: ['Grading', 'Interrupted'],
  Grading: ['Grading', 'Done'],
  Interrupted: ['Planned', 'Failed_infra'],
  Failed_infra: ['Planned'],
  Done: ['Grading'], // only `bench regrade`, and only for a result that is `invalid` (an infrastructure error in grading)
  NotStarted: [],
  StoppedByCap: [],
};

/** States in which the run may still make model calls: it holds its reservation. */
export const ACTIVE = new Set(['Reserved', 'Preparing', 'Running']);
const IN_FLIGHT = new Set(['Reserved', 'Preparing', 'Running', 'Collecting']);
const CALL_PHASES = new Set(['pending', 'settled', 'refused', 'failed']);
const RUN_ID = /^[A-Za-z0-9._:-]{1,128}$/;

export class LedgerError extends Error {
  constructor(message) {
    super(message);
    this.name = 'LedgerError';
  }
}

/**
 * Parse the file. Only a last line without its newline can be torn (the harness died mid-write):
 * it is ignored with a warning. A complete line that is not a record, anywhere, is an error.
 */
export function readLedger(file) {
  let text;
  try {
    text = fs.readFileSync(file, 'utf8');
  } catch (e) {
    if (e.code === 'ENOENT') return { records: [], warnings: [], goodBytes: 0 };
    throw e;
  }
  const records = [];
  const warnings = [];
  const lines = text.split('\n');
  let lastNonEmpty = -1;
  lines.forEach((l, i) => {
    if (l.trim() !== '') lastNonEmpty = i;
  });
  let pos = 0;
  let goodBytes = 0;
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    const hasNewline = i < lines.length - 1;
    const end = pos + Buffer.byteLength(line) + (hasNewline ? 1 : 0);
    if (line.trim() !== '') {
      let rec;
      try {
        rec = JSON.parse(line);
        if (!rec || typeof rec !== 'object' || typeof rec.kind !== 'string') throw new Error('not a record');
      } catch {
        if (i === lastNonEmpty && !hasNewline) {
          warnings.push(`ledger line ${i + 1} is torn and was ignored`);
          break;
        }
        throw new LedgerError(`ledger line ${i + 1} is not a record; the file is corrupt`);
      }
      records.push(rec);
    }
    pos = end;
    goodBytes = end;
  }
  return { records, warnings, goodBytes };
}

function checkRecord(rec) {
  if (!rec || typeof rec !== 'object') throw new LedgerError('a record is an object');
  if (rec.kind === 'setup') return;
  if (typeof rec.run !== 'string' || !RUN_ID.test(rec.run)) throw new LedgerError('a record names its run');
  if (rec.kind === 'state') {
    if (!STATES.includes(rec.state)) throw new LedgerError(`unknown state ${String(rec.state)}`);
    if (rec.state === 'Reserved' && !(Number.isSafeInteger(rec.reservedMicro) && rec.reservedMicro >= 0)) {
      throw new LedgerError('Reserved needs reservedMicro, whole micro-dollars');
    }
    if (rec.state === 'Done' && (typeof rec.result !== 'object' || rec.result === null)) throw new LedgerError('Done needs a result');
  } else if (rec.kind === 'call') {
    if (!CALL_PHASES.has(rec.phase)) throw new LedgerError('unknown call phase');
    if (!Number.isSafeInteger(rec.attempt) || !Number.isSafeInteger(rec.call)) throw new LedgerError('a call record needs attempt and call');
  } else {
    throw new LedgerError(`unknown record kind ${String(rec.kind)}`);
  }
}

export class Ledger {
  /** Open (and lock) the ledger at `file`. A torn last line is cut off so the next record starts clean. */
  static open(file, { now = () => new Date(), onWarning = () => {} } = {}) {
    fs.mkdirSync(path.dirname(file), { recursive: true, mode: 0o700 });
    const lock = Ledger.#lock(`${file}.lock`);
    try {
      const { records, warnings, goodBytes } = readLedger(file);
      for (const w of warnings) onWarning(w);
      if (fs.existsSync(file)) {
        if (warnings.length > 0) fs.truncateSync(file, goodBytes);
        const size = fs.statSync(file).size;
        if (size > 0 && fs.readFileSync(file).at(-1) !== 0x0a) fs.appendFileSync(file, '\n');
      }
      return new Ledger(file, records, lock, now);
    } catch (e) {
      fs.rmSync(lock, { force: true });
      throw e;
    }
  }

  /**
   * Take the lock: write the pid to a temporary file and `link` it to the lock name, which fails
   * if the name exists, so the lock never exists without its pid. A lock of this process is never
   * stale (a second open fails). A lock whose pid is not running is moved away (one rename wins,
   * and the file moved is checked to be the stale one) and then the link is tried again.
   */
  static #lock(lockFile) {
    const mine = `${lockFile}.${process.pid}.${crypto.randomBytes(6).toString('hex')}`;
    fs.writeFileSync(mine, `${process.pid}\n`, { flag: 'wx', mode: 0o600 });
    try {
      for (let attempt = 0; attempt < 4; attempt++) {
        try {
          fs.linkSync(mine, lockFile);
          return lockFile;
        } catch (e) {
          if (e.code !== 'EEXIST') throw e;
        }
        const pid = Ledger.#lockPid(lockFile);
        if (pid === process.pid) throw new LedgerError('the ledger is in use by this process');
        if (pid !== null && Ledger.#running(pid)) throw new LedgerError(`the ledger is in use by process ${pid}`);
        Ledger.#removeStale(lockFile, pid);
      }
      throw new LedgerError('cannot lock the ledger');
    } finally {
      fs.rmSync(mine, { force: true });
    }
  }

  static #lockPid(lockFile) {
    try {
      const pid = Number.parseInt(fs.readFileSync(lockFile, 'utf8'), 10);
      return Number.isSafeInteger(pid) && pid > 0 ? pid : -1;
    } catch (e) {
      if (e.code === 'ENOENT') return null;
      throw e;
    }
  }

  static #running(pid) {
    if (pid < 0) return false;
    try {
      process.kill(pid, 0);
      return true;
    } catch (err) {
      return err.code === 'EPERM';
    }
  }

  /** Move the stale lock away; if what moved is not the stale one (another process won first), put it back. */
  static #removeStale(lockFile, stalePid) {
    if (stalePid === null) return; // already gone: the next link decides
    const moved = `${lockFile}.stale.${process.pid}.${crypto.randomBytes(6).toString('hex')}`;
    try {
      fs.renameSync(lockFile, moved);
    } catch (e) {
      if (e.code === 'ENOENT') return;
      throw e;
    }
    try {
      const was = Number.parseInt(fs.readFileSync(moved, 'utf8'), 10);
      const wasPid = Number.isSafeInteger(was) && was > 0 ? was : -1;
      if (wasPid !== stalePid) {
        try {
          fs.linkSync(moved, lockFile); // someone else's fresh lock: give it back
        } catch {
          // a third process has taken the name; its link failed or won, either way ours is not the owner
        }
      }
    } finally {
      fs.rmSync(moved, { force: true });
    }
  }

  constructor(file, records, lock, now) {
    this.file = file;
    this.records = records;
    this.lock = lock;
    this.now = now;
    this.fd = fs.openSync(file, 'a', 0o600);
    this.states = new Map();
    this.invalid = new Set();
    for (const r of records) if (r.kind === 'state') this.#track(r);
  }

  #track(r) {
    this.states.set(r.run, r.state);
    if (r.state === 'Done') {
      if (r.result?.invalid === true) this.invalid.add(r.run);
      else this.invalid.delete(r.run);
    }
  }

  /** Append one record, fsynced. A state record must follow the lifecycle. */
  append(record) {
    checkRecord(record);
    if (record.kind === 'state') {
      const from = this.states.get(record.run);
      const allowed = from === undefined ? ['Planned'] : TRANSITIONS[from];
      if (!allowed.includes(record.state)) throw new LedgerError(`run ${record.run}: ${from ?? '(new)'} cannot become ${record.state}`);
      if (from === 'Done' && !(record.regrade === true && this.invalid.has(record.run))) {
        throw new LedgerError(`run ${record.run}: a Done run is graded again only when its grade is invalid, by a regrade`);
      }
    }
    const stored = { v: 1, t: this.now().toISOString(), ...record };
    fs.writeSync(this.fd, `${JSON.stringify(stored)}\n`);
    fs.fsyncSync(this.fd);
    this.records.push(stored);
    if (stored.kind === 'state') this.#track(stored);
    return stored;
  }

  state(run) {
    return this.states.get(run);
  }

  fold() {
    return foldRecords(this.records);
  }

  close() {
    if (this.fd !== null) fs.closeSync(this.fd);
    this.fd = null;
    fs.rmSync(this.lock, { force: true });
  }
}

const emptyTokens = () => ({ input: 0, cached: 0, cacheWrite: 0, output: 0, reasoning: 0 });

function newAttempt() {
  return { calls: new Map(), tokens: emptyTokens(), settled: 0, refused: 0, usageMissing: 0, failed: 0 };
}

const attemptSpend = (a) => {
  let sum = 0;
  for (const c of a.calls.values()) sum += c.costMicro ?? c.worstMicro ?? 0;
  return sum;
};

/**
 * Fold the records into one view per run. Spend is the meter's: a settled or failed call counts its
 * cost, a call that was written pending and never settled counts its worst case (the harness may
 * have died with it in flight, and it may have been billed). `tokens` and `calls` sum all attempts
 * of a run, as `spentMicro` does. `wastedMicro` is the spend of attempts
 * that were interrupted, failed in the infrastructure or stopped by the cap.
 */
export function foldRecords(records) {
  const runs = new Map();
  const view = (run) => {
    if (!runs.has(run)) {
      runs.set(run, {
        run,
        state: undefined,
        history: [],
        attempt: 0,
        attempts: [],
        task: null,
        agent: null,
        arm: null,
        block: null,
        source: null,
        language: null,
        reservedMicro: 0,
        reservationActive: false,
        wastedMicro: 0,
        infraFailures: 0,
        interruptions: 0,
        final: false,
        result: null,
        firstAt: null,
        lastAt: null,
      });
    }
    return runs.get(run);
  };

  for (const rec of records) {
    if (rec.kind === 'setup') continue;
    const v = view(rec.run);
    v.firstAt ??= rec.t ?? null;
    v.lastAt = rec.t ?? v.lastAt;
    if (rec.kind === 'state') {
      v.state = rec.state;
      v.history.push(rec.state);
      if (rec.state === 'Planned') {
        v.attempt += 1;
        v.attempts[v.attempt - 1] = newAttempt();
        for (const f of ['task', 'agent', 'arm', 'block', 'source', 'language']) if (rec[f] !== undefined) v[f] = rec[f];
        v.result = null;
      }
      if (rec.state === 'Reserved') {
        v.reservedMicro = rec.reservedMicro;
        v.reservationActive = true;
      } else if (!ACTIVE.has(rec.state)) {
        v.reservationActive = false;
      }
      if (rec.state === 'Interrupted' || rec.state === 'Failed_infra' || rec.state === 'StoppedByCap') {
        // An attempt's spend is wasted once, however many of these states end it (Interrupted, then Failed_infra).
        const a = v.attempts[v.attempt - 1];
        if (a && !a.wasted) {
          a.wasted = true;
          v.wastedMicro += attemptSpend(a);
        }
        if (rec.state === 'Interrupted') v.interruptions += 1;
        if (rec.state === 'Failed_infra') {
          if (rec.final === true) v.final = true;
          else v.infraFailures += 1;
        }
      }
      if (rec.state === 'Done') v.result = rec.result;
    } else if (rec.kind === 'call') {
      const a = (v.attempts[rec.attempt - 1] ??= newAttempt());
      const key = rec.call;
      const prior = a.calls.get(key) ?? {};
      if (rec.phase === 'pending') {
        a.calls.set(key, { ...prior, worstMicro: rec.worstMicro ?? 0 });
      } else if (rec.phase === 'settled' || rec.phase === 'failed') {
        a.calls.set(key, { ...prior, costMicro: rec.costMicro ?? 0 });
        if (rec.phase === 'failed') a.failed += 1;
        else a.settled += 1;
        if (rec.usageMissing) a.usageMissing += 1;
        const u = rec.usage;
        if (u) {
          a.tokens.input += u.input ?? 0;
          a.tokens.cached += u.cached ?? 0;
          a.tokens.cacheWrite += u.cacheWrite ?? 0;
          a.tokens.output += u.output ?? 0;
          a.tokens.reasoning += u.reasoning ?? 0;
        }
      } else if (rec.phase === 'refused') {
        a.refused += 1;
      }
    }
  }

  for (const v of runs.values()) {
    const current = v.attempts[v.attempt - 1];
    v.attemptSpentMicro = current ? attemptSpend(current) : 0;
    v.spentMicro = v.attempts.reduce((s, a) => s + (a ? attemptSpend(a) : 0), 0);
    v.priorSpentMicro = v.spentMicro - v.attemptSpentMicro;
    // Tokens and calls, like the spend, are of all attempts of the run.
    v.tokens = emptyTokens();
    v.calls = { settled: 0, failed: 0, refused: 0, usageMissing: 0 };
    for (const a of v.attempts) {
      if (!a) continue;
      for (const k of Object.keys(v.tokens)) v.tokens[k] += a.tokens[k];
      for (const k of Object.keys(v.calls)) v.calls[k] += a[k];
    }
  }
  return runs;
}

/**
 * What to do with each run when the harness starts, from the fold alone.
 *   skip     Done, NotStarted, StoppedByCap: finished, never repeated
 *   plan     Planned: decide the reservation as for any new run
 *   rerun    Reserved, Preparing, Running, Collecting, Interrupted: start from scratch; the spend of
 *            the interrupted attempt stays in the ledger as wasted
 *   regrade  Grading: grade the saved patch again; never run the agent again
 *   retry    Failed_infra with retries left (at most `maxInfraRetries`)
 *   give-up  Failed_infra with none left, or a run that would be interrupted a third time
 *            (`maxInterruptions`, 2): final, excluded from the rates, its spend reported as wasted
 */
export function resumeActions(folded, { maxInfraRetries = 2, maxInterruptions = 2 } = {}) {
  const out = [];
  for (const v of folded.values()) {
    let action;
    let reason;
    switch (v.state) {
      case 'Done':
      case 'NotStarted':
      case 'StoppedByCap':
        action = 'skip';
        break;
      case 'Planned':
        action = 'plan';
        break;
      case 'Grading':
        action = 'regrade';
        break;
      case 'Failed_infra':
        action = !v.final && v.infraFailures <= maxInfraRetries ? 'retry' : 'give-up';
        break;
      default:
        // A run that was in flight is interrupted now; one already Interrupted has been counted.
        if (v.interruptions + (v.state === 'Interrupted' ? 0 : 1) > maxInterruptions) {
          action = 'give-up';
          reason = 'interrupted';
        } else action = 'rerun';
    }
    out.push({ run: v.run, from: v.state, action, ...(reason ? { reason } : {}), spentMicro: v.spentMicro, wastedMicro: v.wastedMicro });
  }
  return out;
}

/**
 * The state records that bring the ledger to where the actions assume: a run that was in flight
 * becomes Interrupted and then Planned (or, the third time, Interrupted and then a final
 * Failed_infra); a Failed_infra to retry becomes Planned; a Grading run re-enters Grading. Pass the result of `resumeActions`.
 */
export function resumeRecords(actions, folded) {
  const out = [];
  for (const a of actions) {
    const v = folded.get(a.run);
    if (a.action === 'give-up' && a.reason === 'interrupted') {
      if (a.from !== 'Interrupted') out.push({ kind: 'state', run: a.run, state: 'Interrupted', resumed: true });
      out.push({ kind: 'state', run: a.run, state: 'Failed_infra', final: true, reason: 'interrupted_too_often' });
    } else if (a.action === 'rerun') {
      if (IN_FLIGHT.has(a.from)) out.push({ kind: 'state', run: a.run, state: 'Interrupted', resumed: true });
      out.push({ kind: 'state', run: a.run, state: 'Planned', retry: true, task: v.task, agent: v.agent, arm: v.arm, block: v.block, source: v.source, language: v.language });
    } else if (a.action === 'retry') {
      out.push({ kind: 'state', run: a.run, state: 'Planned', retry: true, task: v.task, agent: v.agent, arm: v.arm, block: v.block, source: v.source, language: v.language });
    } else if (a.action === 'regrade') {
      out.push({ kind: 'state', run: a.run, state: 'Grading', regrade: true });
    }
  }
  return out;
}
