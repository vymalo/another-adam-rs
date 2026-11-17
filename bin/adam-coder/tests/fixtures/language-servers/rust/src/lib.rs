pub fn add(a: u32, b: u32) -> u32 {
    a + b
}

pub fn broken() -> u32 {
    add(1, "two")
}
