// One pull for each image, however many ask at once.
//
//   once()   one call for each key: the second asker waits for the first, so two slots never pull the same image twice
//            (a plain Set checked and then written across an `await` lets both through). A failure is not remembered:
//            the next run tries again.
export function once() {
  const calls = new Map();
  return (key, fn) => {
    if (!calls.has(key)) {
      calls.set(
        key,
        Promise.resolve()
          .then(fn)
          .catch((e) => {
            calls.delete(key); // a failure is not remembered: the next run tries again
            throw e;
          }),
      );
    }
    return calls.get(key);
  };
}
