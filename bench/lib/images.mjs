// Task images on the services that use them.
//
//   once()         one call for each key, however many ask at once: the second asker waits for the first, so two slots never pull
//                  the same image twice (a plain Set checked and then written across an `await` lets both through)
//   ensureImage    an image that was built where `bench tasks build` ran is put on a slot's service, from an archive the harness
//                  itself makes on its own side, never a file a container wrote
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

/**
 * `source` has the image (built there); `target` is the slot's service. `archive` is the harness's own file for it.
 * Returns 'present' or 'loaded'. Throws when the source does not have it either.
 */
export async function ensureImage({ ref, source, target, archive, load }) {
  if (await target.hasImage(ref)) return 'present';
  if (!(await source.hasImage(ref))) throw new Error(`the image ${ref} is on no service: build it with bench tasks build`);
  await source.save(ref, archive);
  await load(archive);
  return 'loaded';
}
