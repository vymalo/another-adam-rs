// Several runs at once, one in each slot, a block at a time.
//
// The specs are in block order. Each slot's worker takes the next spec when it is free, so the slots hold runs of the same
// few blocks. The cap is held by reservations (cap.mjs); this file adds whole blocks and the waiting:
//   * before the first run of a block starts, the whole block is held: the budgets of all its runs that are still to start must
//     fit under the cap, with what is spent, what is reserved and what other started blocks still hold. The hold is given back
//     run by run as the block's runs reserve. A block that does not fit is not started: nothing of it is even planned, so a
//     stop at the cap leaves whole blocks, and a later start with room (a higher cap) begins with it;
//   * a block that does not fit while other runs are active waits for one of them to end, because what a run did not spend is
//     free when it closes. A block that does not fit with nothing to wait for ends the schedule.
// Starting is one at a time, in order, and the check and the first reservation run with no `await` between them.
//
// A harness crash (HarnessCrash, a test's stand-in for a kill) ends the schedule: every worker stops taking specs, and the first
// crash is thrown after the workers have settled.
import { resumeActions } from './ledger.mjs';
import { advance, resumeLedger } from './runner.mjs';

const TERMINAL = new Set(['Done', 'StoppedByCap']);
export const MAX_PASSES = 3;

/**
 * @param {object} o
 * @param {object} o.deps          runner deps (ledger, book, meter, stack, agents, config, home, reserveMicro?, pullImage?)
 * @param {object[]} o.specs       runs in the order they start; `slot` is set here
 * @param {number} o.slots
 * @param {(outcome) => void} [o.onOutcome]
 * @returns {Promise<{ outcomes: object[], capHit: boolean, maxActive: number, passes: number }>}
 */
export async function runSchedule({ deps, specs, slots, onOutcome = () => {} }) {
  const outcomes = [];
  const slotList = Array.from({ length: slots }, (_, i) => i + 1);
  let capHit = false;
  let crash = null;
  let maxActive = 0;
  let passes = 0;
  const reserveFor = (spec) => (deps.reserveMicro ? deps.reserveMicro(spec) : deps.config.perRunBudgetMicro);

  for (let pass = 0; pass < MAX_PASSES && !capHit && !crash; pass++) {
    await resumeLedger({ deps, slots: slotList });
    const folded = deps.ledger.fold();
    const queue = specs.filter((s) => !TERMINAL.has(folded.get(s.run)?.state) && folded.get(s.run)?.state !== 'Failed_infra');
    if (queue.length === 0) break;
    passes++;
    let active = 0;
    const waiters = [];
    const wake = () => waiters.splice(0).forEach((resolve) => resolve());

    // Starting is one at a time, in the order of the specs: a run that waits for budget is not passed by the next one.
    // `advance` reserves before its first await, so the reservation is made inside the turn that checked it fits.
    let gate = Promise.resolve();
    const held = new Map(); // block -> what its runs still to start have been promised
    const heldTotal = () => [...held.values()].reduce((x, y) => x + y, 0);
    const toStart = (spec) => {
      const state = folded.get(spec.run)?.state;
      return state === undefined || state === 'Planned';
    };
    const claim = (slot) => {
      const turn = gate.then(async () => {
        if (capHit || crash) return null;
        const spec = queue.shift();
        if (!spec) return null;
        const key = spec.block ?? spec.run;
        if (!held.has(key)) {
          const need = [spec, ...queue.filter((q) => (q.block ?? q.run) === key)].filter(toStart).reduce((sum, q) => sum + reserveFor(q), 0);
          const fits = () => deps.book.committedMicro() + heldTotal() + need <= deps.book.capMicro;
          while (!fits() && active > 0 && !capHit && !crash) await new Promise((resolve) => waiters.push(resolve));
          if (capHit || crash) return null;
          if (!fits()) {
            capHit = true; // this block does not fit and nothing is left to give room: not started, not even planned
            return null;
          }
          held.set(key, need);
        }
        if (toStart(spec)) held.set(key, Math.max(0, held.get(key) - reserveFor(spec)));
        if (held.get(key) === 0 && ![...queue].some((q) => (q.block ?? q.run) === key)) held.delete(key);
        active++;
        maxActive = Math.max(maxActive, active);
        return { running: advance({ deps, spec: { ...spec, slot }, folded }) };
      });
      gate = turn.then(
        () => {},
        () => {},
      );
      return turn;
    };

    const worker = async (slot) => {
      for (;;) {
        let started;
        try {
          started = await claim(slot);
        } catch (e) {
          if (e?.name !== 'HarnessCrash') throw e;
          crash ??= e;
          return;
        }
        if (started === null) return;
        try {
          const outcome = await started.running;
          outcomes.push(outcome);
          onOutcome(outcome);
          if (outcome.state === 'NotStarted' && !outcome.skipped) capHit = true;
        } catch (e) {
          if (e?.name !== 'HarnessCrash') throw e;
          crash ??= e;
        } finally {
          active--;
          wake();
        }
      }
    };
    const results = await Promise.allSettled(slotList.map(worker));
    const failed = results.find((r) => r.status === 'rejected');
    if (failed) throw failed.reason;
    if (crash) break;
    // What failed in the infrastructure is tried again, at most as often as the ledger allows.
    const left = resumeActions(deps.ledger.fold()).filter((a) => a.action === 'retry' && specs.some((s) => s.run === a.run));
    if (left.length === 0) break;
  }
  if (crash) throw crash;
  return { outcomes, capHit, maxActive, passes };
}
