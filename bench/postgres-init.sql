-- One database for each coder of bench/compose.yaml: two coders on one database would claim each other's runs,
-- and a run's files are on the volume of one coder only. The harness drops and creates its slot's database
-- before each run (lib/stack.mjs, resetDatabase), so these only have to exist the first time.
CREATE DATABASE adam_bench_1;
CREATE DATABASE adam_bench_2;
