# kvasilloni — Kvaser canlib32.dll cannelloni shim (Rust)
#
#   make            build 32-bit + 64-bit canlib32.dll (release)
#   make dll32      build only the 32-bit DLL
#   make dll64      build only the 64-bit DLL
#   make test       run host unit tests (wire codec, golden vectors)
#   make verify     confirm the 13 exports are present and undecorated (32-bit)
#   make selftest   end-to-end over vcan1 + cannelloni + wine (needs root/wine)
#   make clean

T32 := i686-pc-windows-gnu
T64 := x86_64-pc-windows-gnu
DLL32 := target/$(T32)/release/canlib32.dll
DLL64 := target/$(T64)/release/canlib32.dll

EXPORTS := canInitializeLibrary canOpenChannel canSetBusParams canBusOn canBusOff \
           canWrite canRead canReadStatus canReadErrorCounters canGetBusStatistics \
           canGetErrorText canGetVersion canClose

.PHONY: all dll32 dll64 test verify selftest clean

all: dll32 dll64

dll32:
	cargo build --release --target $(T32)
	@echo "built $(DLL32)"

dll64:
	cargo build --release --target $(T64)
	@echo "built $(DLL64)"

test:
	cargo test

verify: dll32
	@echo "== 32-bit exports =="
	@for s in $(EXPORTS); do \
	    if objdump -p $(DLL32) | grep -qw $$s; then echo "  ok   $$s"; else echo "  MISS $$s"; fi; \
	done
	@echo -n "decoration check: "; \
	  if objdump -p $(DLL32) | grep -qE "can[A-Za-z]+@[0-9]+"; then echo "FAIL (decorated names present)"; else echo "ok (undecorated)"; fi

selftest: dll32
	bash test/selftest.sh

clean:
	cargo clean
	rm -rf target/selftest
