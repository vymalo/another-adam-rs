int add(int a, int b) { return a + b; }

int broken(void);

int broken(void) {
  return add(1);
}
