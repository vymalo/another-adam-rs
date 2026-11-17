// Several runs at once, one in each slot.
//
// The specs are in block order. Each slot's worker takes the next spec when it is free, so the slots hold
// runs of the same few blocks. The cap is held by reservations (cap.mjs); this file adds the waiting:
//   * a run that does not fit while other runs are active waits for one of them to end, because what it
//     reserved is free again then (a reservation is a ceiling, not a spend);
//   * a run that does not fit and has nothing to wait for really crosses the cap: it is recorded NotStarted and
//     nothing after it is started (blocks are never begun and left half).
// The check "does it fit" and the reservation itself run with no `await` between them, so two slots cannot both
// take the last of the budget (Node runs one of them first).
//
// A harness crash (HarnessCrash, a test's stand-in for a kill) ends the schedule: every worker stops taking
// specs, and the first crash is thrown after the workers have settled.
import { resumeActions } from './ledger.mjs';
import { advance, resumeLedger } from './runner.mjs';

const TERMINAL = new Set(['Done', 'NotStarted', 'StoppedByCap']);
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
    const claim = (slot) => {
      const turn = gate.then(async () => {
        if (capHit || crash) return null;
        const spec = queue.shift();
        if (!spec) return null;
        const view = deps.ledger.fold().get(spec.run);
        if (view === undefined || view.state === 'Planned') {
          const need = reserveFor(spec);
          while (!deps.book.canReserve(need) && active > 0 && !capHit && !crash) await new Promise((resolve) => waiters.push(resolve));
          if (capHit || crash) return null;
        }
        active++;
        maxActive = Math.max(maxActive, active);
        return { running: advance({ deps, spec: { ...spec, slot } }) };
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
