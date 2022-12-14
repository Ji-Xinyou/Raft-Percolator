export RUSTFLAGS=-Dwarnings
export RUST_TEST_THREADS=1
export RUST_BACKTRACE=1

LEVEL_RF=debug
LOG_LEVEL ?= raft=${LEVEL_RF},percolator=info

check:
	cargo fmt --all -- --check
	cargo clippy --all --tests -- -D clippy::all

test: test_others test_2 test_3

test_2: test_2a test_2b test_2c test_2d

test_2a: cargo_test_2a

test_2b: cargo_test_2b

test_2c: cargo_test_2c

test_2d: cargo_test_2d

test_3: test_3a test_3b

test_3a: cargo_test_3a

test_3b: cargo_test_3b

cargo_test_%: check
	RUST_LOG=${LOG_LEVEL} cargo test -p raft -- --nocapture --test $*

test_others: check
	RUST_LOG=${LOG_LEVEL} cargo test -p labrpc -p labcodec -- --nocapture

test_p_%: check
	RUST_LOG=$(LOG_LEVEL) cargo test -p percolator -- --nocapture --test $*

test_percolator: check
	RUST_LOG=${LOG_LEVEL} cargo test -p percolator -- --nocapture
