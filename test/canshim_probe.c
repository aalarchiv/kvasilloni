/* SPDX-License-Identifier: LGPL-3.0-or-later */
/*
 * canshim_probe.exe - the Windows-side actor for the end-to-end selftest.
 *
 * Loads canlib32.dll the same way the real app does (LoadLibrary +
 * GetProcAddress, by undecorated name), opens a channel, writes one frame, then
 * polls canRead for a few seconds and prints every frame it receives.
 *
 * Run under wine by test/selftest.sh, with KVASILLONI_* env pointing at cannelloni.
 *
 *   canshim_probe.exe <hex_id> <ext|std> <hex_data_bytes...>
 *   e.g.  canshim_probe.exe 0x18EEFF00 ext DE AD BE EF
 *
 * Extended-export modes (retargeting coverage, epic kvasilloni-5yp):
 *   canshim_probe.exe --enum
 *       print canGetNumberOfChannels + canGetChannelData(CHANNEL_NAME).
 *   canshim_probe.exe --accept <hex_accept_id> <std|ext> <hex_id> <std|ext>
 *       canAccept only <accept_id>, then poll canRead and print every RX id.
 *   canshim_probe.exe --notify <hex_id> <std|ext> <hex_data...>
 *       register a canSetNotify(canNOTIFY_RX) callback, poll, print the count.
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

#define canFILTER_SET_CODE_STD 3
#define canFILTER_SET_MASK_STD 4
#define canFILTER_SET_CODE_EXT 5
#define canFILTER_SET_MASK_EXT 6
#define canCHANNELDATA_CHANNEL_NAME 13
#define canNOTIFY_RX 0x0001

typedef void (__stdcall *fn_init)(void);
typedef int  (__stdcall *fn_open)(int, int);
typedef int  (__stdcall *fn_setbus)(int, long, unsigned, unsigned, unsigned, unsigned, unsigned);
typedef int  (__stdcall *fn_buson)(int);
typedef int  (__stdcall *fn_write)(int, long, void*, unsigned, unsigned);
typedef int  (__stdcall *fn_read)(int, long*, void*, unsigned*, unsigned*, unsigned long*);
typedef int  (__stdcall *fn_close)(int);
typedef int  (__stdcall *fn_numchan)(int*);
typedef int  (__stdcall *fn_chandata)(int, int, void*, size_t);
typedef int  (__stdcall *fn_accept)(int, long, unsigned);
typedef int  (__stdcall *fn_setnotify)(int, void*, unsigned, void*);

/* canNotifyData: only the RX-relevant prefix matters here (see canlib.h). */
typedef struct { void *tag; int eventType; long id; unsigned long time; } notify_data;
static volatile long g_notify_count = 0;
static void __stdcall notify_cb(notify_data *d) {
    (void)d;
    g_notify_count++;
}

/* Forward declarations of the alternate entry points. */
static int run_enum(HMODULE dll);
static int run_accept(HMODULE dll, int argc, char **argv);
static int run_notify(HMODULE dll, int argc, char **argv);
static int run_multi(HMODULE dll, int argc, char **argv);

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

    /* Alternate modes for the extended exports (retargeting coverage). */
    if (argc > 1 && !strcmp(argv[1], "--enum"))   return run_enum(dll);
    if (argc > 1 && !strcmp(argv[1], "--accept")) return run_accept(dll, argc, argv);
    if (argc > 1 && !strcmp(argv[1], "--notify")) return run_notify(dll, argc, argv);
    if (argc > 1 && !strcmp(argv[1], "--multi"))  return run_multi(dll, argc, argv);

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

/* --enum: enumerate channels and print the count + first channel name. */
static int run_enum(HMODULE dll)
{
    fn_init     p_init     = (fn_init)    GetProcAddress(dll, "canInitializeLibrary");
    fn_numchan  p_numchan  = (fn_numchan) GetProcAddress(dll, "canGetNumberOfChannels");
    fn_chandata p_chandata = (fn_chandata)GetProcAddress(dll, "canGetChannelData");
    int count = -1, st;
    char name[64];
    if (!p_numchan || !p_chandata) { fprintf(stderr, "PROBE: enum exports missing\n"); return 2; }
    if (p_init) p_init();
    st = p_numchan(&count);
    printf("PROBE: ENUM count=%d st=%d\n", count, st);
    memset(name, 0, sizeof name);
    st = p_chandata(0, canCHANNELDATA_CHANNEL_NAME, name, sizeof name);
    printf("PROBE: ENUM name=\"%s\" st=%d\n", name, st);
    fflush(stdout);
    FreeLibrary(dll);
    return 0;
}

/* --accept <hex_accept_id> <std|ext> : accept only that id, print every RX. */
static int run_accept(HMODULE dll, int argc, char **argv)
{
    fn_init   p_init   = (fn_init)  GetProcAddress(dll, "canInitializeLibrary");
    fn_open   p_open   = (fn_open)  GetProcAddress(dll, "canOpenChannel");
    fn_setbus p_setbus = (fn_setbus)GetProcAddress(dll, "canSetBusParams");
    fn_buson  p_buson  = (fn_buson) GetProcAddress(dll, "canBusOn");
    fn_read   p_read   = (fn_read)  GetProcAddress(dll, "canRead");
    fn_accept p_accept = (fn_accept)GetProcAddress(dll, "canAccept");
    fn_close  p_close  = (fn_close) GetProcAddress(dll, "canClose");
    long accept_id = (argc > 2) ? strtol(argv[2], NULL, 0) : 0;
    const char *spec = (argc > 3) ? argv[3] : "std";
    int ext = strstr(spec, "ext") ? 1 : 0;
    int h, loops;
    if (!p_open || !p_read || !p_accept) { fprintf(stderr, "PROBE: accept exports missing\n"); return 2; }
    if (p_init) p_init();
    h = p_open(0, 0);
    if (h < 0) { fprintf(stderr, "PROBE: canOpenChannel failed %d\n", h); return 3; }
    if (p_setbus) p_setbus(h, 250000, 0, 0, 0, 0, 0);
    if (p_buson) p_buson(h);
    /* exact-match acceptance: code = id, mask = all-ones for the id class */
    if (ext) {
        p_accept(h, accept_id, canFILTER_SET_CODE_EXT);
        p_accept(h, 0x1FFFFFFF, canFILTER_SET_MASK_EXT);
    } else {
        p_accept(h, accept_id, canFILTER_SET_CODE_STD);
        p_accept(h, 0x7FF, canFILTER_SET_MASK_STD);
    }
    printf("PROBE: ACCEPT only id=0x%lX (%s)\n", accept_id, ext ? "ext" : "std");
    fflush(stdout);
    for (loops = 0; loops < 500; loops++) {
        long rid; unsigned rdlc, rflag; unsigned long t;
        unsigned char rbuf[8];
        if (p_read(h, &rid, rbuf, &rdlc, &rflag, &t) == 0) {
            printf("PROBE: RX id=0x%lX dlc=%u flag=0x%x\n", rid, rdlc, rflag);
            fflush(stdout);
        } else {
            Sleep(10);
        }
    }
    if (p_close) p_close(h);
    FreeLibrary(dll);
    return 0;
}

/* --notify <hex_id> <std|ext> <hex_data...> : count canNOTIFY_RX callbacks. */
static int run_notify(HMODULE dll, int argc, char **argv)
{
    fn_init      p_init      = (fn_init)     GetProcAddress(dll, "canInitializeLibrary");
    fn_open      p_open      = (fn_open)     GetProcAddress(dll, "canOpenChannel");
    fn_setbus    p_setbus    = (fn_setbus)   GetProcAddress(dll, "canSetBusParams");
    fn_buson     p_buson     = (fn_buson)    GetProcAddress(dll, "canBusOn");
    fn_setnotify p_setnotify = (fn_setnotify)GetProcAddress(dll, "canSetNotify");
    fn_close     p_close     = (fn_close)    GetProcAddress(dll, "canClose");
    int h, loops;
    if (!p_open || !p_setnotify) { fprintf(stderr, "PROBE: notify exports missing\n"); return 2; }
    if (p_init) p_init();
    h = p_open(0, 0);
    if (h < 0) { fprintf(stderr, "PROBE: canOpenChannel failed %d\n", h); return 3; }
    if (p_setbus) p_setbus(h, 250000, 0, 0, 0, 0, 0);
    if (p_buson) p_buson(h);
    p_setnotify(h, (void*)notify_cb, canNOTIFY_RX, NULL);
    printf("PROBE: NOTIFY armed\n");
    fflush(stdout);
    /* wait while injected frames arrive and fire the callback */
    for (loops = 0; loops < 500; loops++) Sleep(10);
    p_setnotify(h, NULL, 0, NULL); /* disarm before close */
    printf("PROBE: NOTIFY count=%ld\n", g_notify_count);
    fflush(stdout);
    if (p_close) p_close(h);
    FreeLibrary(dll);
    return 0;
}

/* --multi <hex_id_a> <hex_id_b> : open TWO channels in one process, verify the
 * handles are distinct + non-negative, TX on both, and that the first channel
 * (bound to the configured local port cannelloni replies to) still receives.
 * Exercises the handle table (kvasilloni-j83) and the UDP ephemeral-port
 * fallback for the second open (kvasilloni-iai) together. */
static int run_multi(HMODULE dll, int argc, char **argv)
{
    fn_init  p_init  = (fn_init) GetProcAddress(dll, "canInitializeLibrary");
    fn_open  p_open  = (fn_open) GetProcAddress(dll, "canOpenChannel");
    fn_setbus p_setbus = (fn_setbus)GetProcAddress(dll, "canSetBusParams");
    fn_buson p_buson = (fn_buson)GetProcAddress(dll, "canBusOn");
    fn_write p_write = (fn_write)GetProcAddress(dll, "canWrite");
    fn_read  p_read  = (fn_read) GetProcAddress(dll, "canRead");
    fn_close p_close = (fn_close)GetProcAddress(dll, "canClose");
    long ida = (argc > 2) ? strtol(argv[2], NULL, 0) : 0x18EEFF10;
    long idb = (argc > 3) ? strtol(argv[3], NULL, 0) : 0x18EEFF11;
    unsigned char d[2] = { 0xAA, 0xBB };
    int ha, hb, loops;
    if (!p_open || !p_write || !p_read) { fprintf(stderr, "PROBE: multi exports missing\n"); return 2; }
    if (p_init) p_init();
    ha = p_open(0, 0);
    hb = p_open(1, 0);  /* second open: configured UDP port is busy -> ephemeral */
    printf("PROBE: MULTI ha=%d hb=%d distinct=%d\n",
           ha, hb, (ha >= 0 && hb >= 0 && ha != hb) ? 1 : 0);
    fflush(stdout);
    if (ha < 0 || hb < 0) { fprintf(stderr, "PROBE: a multi open failed\n"); return 3; }
    if (p_setbus) { p_setbus(ha, 250000, 0, 0, 0, 0, 0); p_setbus(hb, 250000, 0, 0, 0, 0, 0); }
    if (p_buson) { p_buson(ha); p_buson(hb); }
    Sleep(300);
    printf("PROBE: TXa id=0x%lX -> st=%d\n", ida, p_write(ha, ida, d, 2, canMSG_EXT));
    printf("PROBE: TXb id=0x%lX -> st=%d\n", idb, p_write(hb, idb, d, 2, canMSG_EXT));
    fflush(stdout);
    for (loops = 0; loops < 500; loops++) {
        long rid; unsigned rdlc, rflag; unsigned long t; unsigned char rbuf[8];
        if (p_read(ha, &rid, rbuf, &rdlc, &rflag, &t) == 0) {
            printf("PROBE: RXa id=0x%lX dlc=%u\n", rid, rdlc);
            fflush(stdout);
        } else {
            Sleep(10);
        }
    }
    if (p_close) { p_close(ha); p_close(hb); }
    FreeLibrary(dll);
    return 0;
}
