/*
 * canshim_probe.exe — the Windows-side actor for the end-to-end selftest.
 *
 * Loads canlib32.dll the same way the real app does (LoadLibrary +
 * GetProcAddress, by undecorated name), opens a channel, writes one frame, then
 * polls canRead for a few seconds and prints every frame it receives.
 *
 * Run under wine by test/selftest.sh, with CANSHIM_* env pointing at cannelloni.
 *
 *   canshim_probe.exe <hex_id> <ext|std> <hex_data_bytes...>
 *   e.g.  canshim_probe.exe 0x18EEFF00 ext DE AD BE EF
 */
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#define canMSG_STD 0x0002
#define canMSG_EXT 0x0004
#define canFDMSG_FDF 0x010000
#define canFDMSG_BRS 0x020000
#define canFDMSG_ESI 0x040000
#define canERR_NOMSG (-2)

typedef void (__stdcall *fn_init)(void);
typedef int  (__stdcall *fn_open)(int, int);
typedef int  (__stdcall *fn_setbus)(int, long, unsigned, unsigned, unsigned, unsigned, unsigned);
typedef int  (__stdcall *fn_buson)(int);
typedef int  (__stdcall *fn_write)(int, long, void*, unsigned, unsigned);
typedef int  (__stdcall *fn_read)(int, long*, void*, unsigned*, unsigned*, unsigned long*);
typedef int  (__stdcall *fn_close)(int);

int main(int argc, char **argv)
{
    HMODULE dll;
    fn_init  p_init;
    fn_open  p_open;
    fn_setbus p_setbus;
    fn_buson p_buson;
    fn_write p_write;
    fn_read  p_read;
    fn_close p_close;
    int h, st, i, loops;
    long id = (argc > 1) ? strtol(argv[1], NULL, 0) : 0x123;
    /* argv[2] is a flag spec: any of ext/std/fd/brs as substrings, e.g. "extfdbrs" */
    const char *spec = (argc > 2) ? argv[2] : "std";
    unsigned flag = strstr(spec, "ext") ? canMSG_EXT : canMSG_STD;
    unsigned is_fd = strstr(spec, "fd") ? 1 : 0;
    unsigned char data[64]; unsigned dlc = 0;
    if (is_fd) {
        flag |= canFDMSG_FDF;
        if (strstr(spec, "brs")) flag |= canFDMSG_BRS;
    }
    for (i = 3; i < argc && dlc < 64; i++) data[dlc++] = (unsigned char)strtol(argv[i], NULL, 16);

    dll = LoadLibraryA("canlib32.dll");
    if (!dll) { fprintf(stderr, "PROBE: LoadLibrary failed %lu\n", GetLastError()); return 2; }

#define GET(v, t, n) v = (t)GetProcAddress(dll, n); \
    if (!v) { fprintf(stderr, "PROBE: missing export %s\n", n); return 2; }
    GET(p_init, fn_init, "canInitializeLibrary");
    GET(p_open, fn_open, "canOpenChannel");
    GET(p_setbus, fn_setbus, "canSetBusParams");
    GET(p_buson, fn_buson, "canBusOn");
    GET(p_write, fn_write, "canWrite");
    GET(p_read, fn_read, "canRead");
    GET(p_close, fn_close, "canClose");
#undef GET

    p_init();
    h = p_open(0, 0);
    if (h < 0) { fprintf(stderr, "PROBE: canOpenChannel failed %d\n", h); return 3; }
    p_setbus(h, 250000, 0, 0, 0, 0, 0);
    p_buson(h);
    Sleep(300);   /* let TCP negotiate / let cannelloni settle */

    st = p_write(h, id, data, dlc, flag);
    printf("PROBE: TX id=0x%lX dlc=%u flag=0x%x -> st=%d\n", id, dlc, flag, st);
    fflush(stdout);

    /* poll for ~5s for inbound frames */
    for (loops = 0; loops < 500; loops++) {
        long rid; unsigned rdlc, rflag; unsigned long t;
        unsigned char rbuf[8];
        int r = p_read(h, &rid, rbuf, &rdlc, &rflag, &t);
        if (r == 0) {
            unsigned j;
            printf("PROBE: RX id=0x%lX dlc=%u flag=0x%x data=", rid, rdlc, rflag);
            for (j = 0; j < rdlc && j < 8; j++) printf("%02X", rbuf[j]);
            printf("\n"); fflush(stdout);
        } else {
            Sleep(10);
        }
    }
    p_close(h);
    FreeLibrary(dll);
    return 0;
}
