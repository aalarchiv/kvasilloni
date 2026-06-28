# kvasilloni - Kvaser canlib32.dll cannelloni shim (Rust)
#
#   make            build 32-bit + 64-bit canlib32.dll (release)
#   make dll32      build only the 32-bit DLL
#   make dll64      build only the 64-bit DLL
#   make test       run host unit tests (wire codec, golden vectors)
#   make race       race-detect the transport concurrency under ThreadSanitizer
#   make verify     confirm the exports are present and undecorated (32-bit)
#   make selftest   end-to-end over vcan1 + cannelloni + wine (32-bit DLL)
#   make selftest64 same end-to-end suite against the 64-bit DLL (wine wow64)
#   make clean

T32 := i686-pc-windows-gnu
T64 := x86_64-pc-windows-gnu
DLL32 := target/$(T32)/release/canlib32.dll
DLL64 := target/$(T64)/release/canlib32.dll

EXPORTS := canInitializeLibrary canOpenChannel canSetBusParams canBusOn canBusOff \
           canWrite canRead canReadStatus canReadErrorCounters canGetBusStatistics \
           canGetErrorText canGetVersion canClose \
           canFlushReceiveQueue canFlushTransmitQueue \
           canSetBusOutputControl canGetBusOutputControl \
           canReadWait canReadSync canWriteWait canWriteSync \
           canIoCtl canAccept canObjBufSetFilter \
           canGetNumberOfChannels canGetChannelData canSetNotify

.PHONY: all dll32 dll64 test race verify selftest selftest64 clean

all: dll32 dll64

dll32:
	cargo build --release --target $(T32)
	@echo "built $(DLL32)"

dll64:
	cargo build --release --target $(T64)
	@echo "built $(DLL64)"

test:
	cargo test

# Race-detect the transport concurrency (Shared ring + Conn teardown) under
# ThreadSanitizer. -Z build-std rebuilds std with TSan instrumentation so its
# Mutex/Condvar are seen too; halt_on_error makes any data race a hard failure.
# Needs the nightly toolchain + rust-src. See kvasilloni-lw6.3 and the
# race-detection test block in src/transport.rs.
race:
	rustup component add rust-src --toolchain nightly
	TSAN_OPTIONS="halt_on_error=1" RUSTFLAGS="-Zsanitizer=thread" \
	    cargo +nightly test -Z build-std --target x86_64-unknown-linux-gnu --lib transport::

verify: dll32
	@echo "== 32-bit exports =="
	@for s in $(EXPORTS); do \
	    if objdump -p $(DLL32) | grep -qw $$s; then echo "  ok   $$s"; else echo "  MISS $$s"; fi; \
	done
	@echo -n "decoration check: "; \
	  if objdump -p $(DLL32) | grep -qE "can[A-Za-z]+@[0-9]+"; then echo "FAIL (decorated names present)"; else echo "ok (undecorated)"; fi

selftest: dll32
	bash test/selftest.sh

selftest64: dll64
	SELFTEST_ARCH=64 bash test/selftest.sh

clean:
	cargo clean
	rm -rf target/selftest
