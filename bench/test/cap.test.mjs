import assert from 'node:assert/strict';
import { test } from 'node:test';
import { CapBook } from '../lib/cap.mjs';
import { foldRecords } from '../lib/ledger.mjs';

const CAP = 10_000_000;

test('a reservation fits exactly at the cap, and one micro-dollar over is refused', () => {
  const book = new CapBook(CAP);
  assert.equal(book.reserve('a', 6_000_000), true);
  assert.equal(book.reserve('b', 4_000_001), false, 'one over');
  assert.equal(book.committedMicro(), 6_000_000, 'a refusal changes nothing');
  assert.equal(book.reserve('b', 4_000_000), true, 'exactly at the cap');
  assert.equal(book.reserve('c', 1), false);
});

test('closing a run frees the unused part of its reservation and keeps its spend', () => {
  const book = new CapBook(CAP);
  book.reserve('a', 6_000_000);
  book.reserve('b', 4_000_000);
  assert.deepEqual(book.beginCall('a', 900_000), { ok: true });
  book.endCall('a', 900_000, 1_000_000 - 100_000); // actual 900_000
  book.endCall('a', 0, 100_000); // another settled call: spend 1_000_000
  book.close('a');
  assert.equal(book.committedMicro(), 1_000_000 + 4_000_000);
  assert.equal(book.reserve('c', 5_000_001), false);
  assert.equal(book.reserve('c', 5_000_000), true);
});

test('a call must fit what is left of its run, counting the calls in flight; exactly is allowed', () => {
  const book = new CapBook(CAP);
  book.reserve('a', 1_000);
  assert.deepEqual(book.beginCall('a', 600), { ok: true });
  assert.deepEqual(book.beginCall('a', 401), { ok: false, reason: 'run-budget' }, '600 in flight + 401 > 1000');
  assert.deepEqual(book.beginCall('a', 400), { ok: true }, '600 + 400 = 1000');
  assert.deepEqual(book.beginCall('a', 1), { ok: false, reason: 'run-budget' });
  book.endCall('a', 600, 100); // the first call cost 100, not 600
  assert.deepEqual(book.beginCall('a', 500), { ok: true }, '100 spent + 400 in flight + 500 = 1000');
  assert.equal(book.runView('a').spentMicro, 100);
});

test('a run that is not active may not call', () => {
  const book = new CapBook(CAP);
  assert.deepEqual(book.beginCall('ghost', 1), { ok: false, reason: 'inactive' });
  book.reserve('a', 100);
  book.close('a');
  assert.deepEqual(book.beginCall('a', 1), { ok: false, reason: 'inactive' });
});

test('the global stop: actual spend reaching the cap refuses every call', () => {
  const book = new CapBook(1_000_000);
  book.reserve('a', 1_000_000);
  assert.equal(book.globalStop(), false);
  assert.deepEqual(book.beginCall('a', 400_000), { ok: true });
  book.endCall('a', 400_000, 1_000_000); // the endpoint reported more than the worst case
  assert.equal(book.globalStop(), true);
  assert.deepEqual(book.beginCall('a', 1), { ok: false, reason: 'cap' });
  assert.equal(book.canReserve(1), false);
});

test('spend above the reservation still counts against the cap', () => {
  const book = new CapBook(CAP);
  book.reserve('a', 1_000);
  book.endCall('a', 0, 1_500);
  assert.equal(book.committedMicro(), 1_500);
});

test('a second attempt of a run adds to what the first spent', () => {
  const book = new CapBook(CAP);
  book.reserve('a', 1_000);
  book.endCall('a', 0, 700);
  book.close('a');
  assert.equal(book.reserve('a', 1_000), true);
  assert.equal(book.runView('a').spentMicro, 700);
  assert.equal(book.committedMicro(), 700 + 1_000);
  assert.throws(() => book.reserve('a', 1), /already holds/);
});

test('a book built from the ledger fold holds the reservations of the runs still active', () => {
  const folded = foldRecords([
    { kind: 'state', run: 'a', state: 'Planned' },
    { kind: 'state', run: 'a', state: 'Reserved', reservedMicro: 3_000_000 },
    { kind: 'state', run: 'a', state: 'Preparing' },
    { kind: 'state', run: 'a', state: 'Running' },
    { kind: 'call', run: 'a', attempt: 1, call: 1, phase: 'pending', worstMicro: 50 },
    { kind: 'call', run: 'a', attempt: 1, call: 1, phase: 'settled', costMicro: 40 },
    { kind: 'state', run: 'b', state: 'Planned' },
    { kind: 'state', run: 'b', state: 'Reserved', reservedMicro: 2_000_000 },
    { kind: 'state', run: 'b', state: 'Preparing' },
    { kind: 'state', run: 'b', state: 'Running' },
    { kind: 'call', run: 'b', attempt: 1, call: 1, phase: 'settled', costMicro: 1_000 },
    { kind: 'state', run: 'b', state: 'Collecting' },
  ]);
  const book = CapBook.fromFold(CAP, folded);
  assert.equal(book.spentMicro(), 1_040);
  assert.equal(book.committedMicro(), 3_000_000 + 1_000, 'a holds 3 USD, b only what it spent');
});

test('a cap and a reservation are whole micro-dollars', () => {
  assert.throws(() => new CapBook(1.5), RangeError);
  assert.throws(() => new CapBook(CAP).reserve('a', 0.5), RangeError);
});
