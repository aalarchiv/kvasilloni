# kvasilloni — canlib32.dll cannelloni shim
#
#   make            build 32-bit + 64-bit canlib32.dll
#   make test       build & run the host wire-codec cross-validation test
#   make verify     confirm the 13 exports are present and undecorated
#   make probe      build the wine end-to-end probe exe (32-bit)
#   make selftest   end-to-end over vcan0 + cannelloni + wine (needs root/wine)
#   make clean

CC32 := i686-w64-mingw32-gcc
CC64 := x86_64-w64-mingw32-gcc
HOSTCXX := g++

SRC      := src/canlib32_shim.c src/cannelloni_wire.c
DEF      := src/canlib32.def
CFLAGS   := -O2 -Wall -Wextra -Isrc
LDFLAGS  := -shared -Wl,--kill-at -lws2_32 -static-libgcc

OUT32 := build/canlib32.dll
OUT64 := build/canlib32-x64.dll

CANNELLONI := ../../refs/cannelloni

.PHONY: all test verify probe selftest clean dirs

all: dirs $(OUT32) $(OUT64)

dirs:
	@mkdir -p build

$(OUT32): $(SRC) $(DEF) src/cannelloni_wire.h | dirs
	$(CC32) $(CFLAGS) $(SRC) $(DEF) -o $@ $(LDFLAGS)
	@echo "built $@ (32-bit)"

$(OUT64): $(SRC) $(DEF) src/cannelloni_wire.h | dirs
	$(CC64) $(CFLAGS) $(SRC) $(DEF) -o $@ $(LDFLAGS)
	@echo "built $@ (64-bit)"

# ---- host wire-codec test: cross-validate vs cannelloni's own parser/decoder --
test: dirs
	$(HOSTCXX) -O2 -Wall -Isrc -I$(CANNELLONI) \
	    test/wire_test.cpp src/cannelloni_wire.c \
	    $(CANNELLONI)/parser.cpp $(CANNELLONI)/decoder.cpp \
	    -o build/wire_test
	./build/wire_test

verify: all
	@echo "== 32-bit exports =="; objdump -p $(OUT32) | grep -A20 "Export Address Table" | grep -E "can[A-Z]" || true
	@for s in canInitializeLibrary canOpenChannel canSetBusParams canBusOn canBusOff \
	          canWrite canRead canReadStatus canReadErrorCounters canGetBusStatistics \
	          canGetErrorText canGetVersion canClose; do \
	    if objdump -p $(OUT32) | grep -qw $$s; then echo "  ok   $$s"; else echo "  MISS $$s"; fi; \
	done
	@echo "== decoration check (must find NO name@N) =="; \
	  if objdump -p $(OUT32) | grep -E "can[A-Za-z]+@[0-9]+"; then echo "  FAIL: decorated names present"; else echo "  ok: undecorated"; fi

probe: dirs
	$(CC32) -O2 -Wall test/canshim_probe.c -o build/canshim_probe.exe

selftest: all probe
	bash test/selftest.sh

clean:
	rm -rf build
