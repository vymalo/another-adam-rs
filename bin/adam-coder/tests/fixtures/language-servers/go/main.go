package main

func add(a int, b int) int {
	return a + b
}

func broken() int {
	return add(1, "two")
}

func main() {
	_ = broken()
}
