export function add(a: number, b: number): number {
  return a + b;
}

export const broken: number = add(1, "two");
