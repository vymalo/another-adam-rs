// The cap on spend, as reservations.
//
// A run reserves its budget before it starts. A run holds its reservation while it is active
// (Reserved, Preparing, Running) and its actual spend after. So
//   committed = spend of attempts that ended + for each active run max(reservation, spend)
// and a run may start only if committed + its reservation <= cap (exactly at the cap is allowed,
// one micro-dollar over is not). When a run closes, the unused part of its reservation is free.
// Each call is checked against its run's reservation with its worst case. If the actual spend
// reaches the cap anyway (a price edited mid-run, an endpoint reporting more than the worst case),
// every call is refused: the global stop.

export class CapBook {
  constructor(capMicro) {
    if (!Number.isSafeInteger(capMicro) || capMicro < 0) throw new RangeError('cap must be whole micro-dollars');
    this.capMicro = capMicro;
    this.runs = new Map();
  }

  /** A book from the folded ledger (ledger.mjs `foldRecords`). */
  static fromFold(capMicro, folded) {
    const book = new CapBook(capMicro);
    for (const view of folded.values()) {
      book.runs.set(view.run, {
        prior: view.priorSpentMicro,
        attempt: view.attemptSpentMicro,
        pending: 0,
        reserved: view.reservationActive ? view.reservedMicro : 0,
        active: view.reservationActive,
      });
    }
    return book;
  }

  #entry(run) {
    let e = this.runs.get(run);
    if (!e) {
      e = { prior: 0, attempt: 0, pending: 0, reserved: 0, active: false };
      this.runs.set(run, e);
    }
    return e;
  }

  /** Spend of every run, settled calls only. */
  spentMicro() {
    let sum = 0;
    for (const e of this.runs.values()) sum += e.prior + e.attempt;
    return sum;
  }

  /** What the cap has to cover: closed spend plus, for each active run, its reservation. */
  committedMicro() {
    let sum = 0;
    for (const e of this.runs.values()) {
      sum += e.prior + (e.active ? Math.max(e.reserved, e.attempt) : e.attempt);
    }
    return sum;
  }

  canReserve(amountMicro) {
    return this.committedMicro() + amountMicro <= this.capMicro;
  }

  /** Reserve for a new attempt of `run`. False, and nothing changes, when it would cross the cap. */
  reserve(run, amountMicro) {
    if (!Number.isSafeInteger(amountMicro) || amountMicro < 0) throw new RangeError('reservation must be whole micro-dollars');
    const e = this.#entry(run);
    if (e.active) throw new Error(`run ${run} already holds a reservation`);
    if (!this.canReserve(amountMicro)) return false;
    e.prior += e.attempt;
    e.attempt = 0;
    e.pending = 0;
    e.reserved = amountMicro;
    e.active = true;
    return true;
  }

  /** The unused part of the reservation is free from now on; the spend stays. */
  close(run) {
    const e = this.#entry(run);
    e.active = false;
    e.pending = 0;
  }

  /** Spent so far, in the run's reservation, and in flight. */
  runView(run) {
    const e = this.#entry(run);
    return { spentMicro: e.prior + e.attempt, attemptMicro: e.attempt, pendingMicro: e.pending, reservedMicro: e.reserved, active: e.active };
  }

  /** The global stop: actual spend has reached the cap. */
  globalStop() {
    return this.spentMicro() >= this.capMicro;
  }

  /** Before a call: is its worst case inside what is left of the run's reservation? */
  beginCall(run, worstMicro) {
    const e = this.#entry(run);
    if (!e.active) return { ok: false, reason: 'inactive' };
    if (this.globalStop()) return { ok: false, reason: 'cap' };
    if (e.attempt + e.pending + worstMicro > e.reserved) return { ok: false, reason: 'run-budget' };
    e.pending += worstMicro;
    return { ok: true };
  }

  /** After a call: the worst case is no longer in flight, the actual cost is spent. */
  endCall(run, worstMicro, actualMicro) {
    const e = this.#entry(run);
    e.pending = Math.max(0, e.pending - worstMicro);
    e.attempt += actualMicro;
  }

  snapshot() {
    return { capMicro: this.capMicro, spentMicro: this.spentMicro(), committedMicro: this.committedMicro() };
  }
}
