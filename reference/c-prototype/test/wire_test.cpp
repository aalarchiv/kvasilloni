/*
 * wire_test.c (compiled as C++) — cross-validate our cannelloni_wire codec
 * against cannelloni's OWN parser.cpp / decoder.cpp, compiled natively. This is
 * a true interop check: no wine, no network, no root.
 *
 * Directions exercised:
 *   1. our cw_build_udp  -> cannelloni parseFrames   (UDP, we encode / they decode)
 *   2. cannelloni buildPacket -> our cw_parse_udp    (UDP, they encode / we decode)
 *   3. our cw_encode_frame -> cannelloni decodeFrame (TCP stream, we enc / they dec)
 *   4. cannelloni encodeFrame -> our cw_decode_stream(TCP stream, they enc / we dec)
 */
#include <cstdio>
#include <cstring>
#include <cstdint>
#include <list>
#include <vector>
#include <linux/can.h>

extern "C" {
#include "cannelloni_wire.h"
}
#include "parser.h"
#include "decoder.h"

static int g_fail = 0;
#define CHECK(cond, msg) do { if(!(cond)){ printf("  FAIL: %s\n", msg); g_fail++; } } while(0)

/* ---- a test case in SocketCAN terms ---- */
struct TC { const char *name; uint32_t can_id; uint8_t len; const uint8_t *data; };

static int frames_equal(uint32_t id1, uint8_t len1, const uint8_t *d1,
                        uint32_t id2, uint8_t len2, const uint8_t *d2)
{
    if (id1 != id2) return 0;
    /* RTR frames carry no data; cannelloni's TCP streaming decoder also drops
     * the DLC (decoder.cpp sets len=0 for RTR), while its UDP parser keeps it.
     * So for RTR only the can_id (incl. RTR/EFF flags) is a reliable invariant. */
    if (id1 & CW_CAN_RTR_FLAG) return 1;
    if (len1 != len2) return 0;
    if (len1 && memcmp(d1, d2, len1) != 0) return 0;
    return 1;
}

/* ---------- direction 1: our UDP encode -> cannelloni parseFrames ---------- */
struct ParseCtx { canfd_frame got; int n; };
static void run_dir1(const TC &tc)
{
    cw_frame_t f; memset(&f, 0, sizeof f);
    f.can_id = tc.can_id; f.len = tc.len; f.fd = 0;
    if (tc.data) memcpy(f.data, tc.data, tc.len);

    uint8_t pkt[256];
    size_t n = cw_build_udp(pkt, &f, 7);

    /* cannelloni's parser: allocator hands out a frame, receiver captures it */
    static canfd_frame storage;
    static int count;
    count = 0;
    auto alloc = []() -> canfd_frame* { return &storage; };
    auto recv  = [](canfd_frame *fr, bool ok) { (void)fr; if (ok) count++; };
    parseFrames((uint16_t)n, pkt, alloc, recv);

    CHECK(count == 1, "dir1: cannelloni parsed exactly 1 frame");
    CHECK(frames_equal(tc.can_id, tc.len, tc.data,
                       storage.can_id, (uint8_t)(storage.len & 0x7F), storage.data),
          tc.name);
}

/* ---------- direction 2: cannelloni buildPacket -> our cw_parse_udp -------- */
struct CapCtx { cw_frame_t f; int n; };
static void cap_cb(void *u, const cw_frame_t *fr) { CapCtx *c = (CapCtx*)u; c->f = *fr; c->n++; }
static void run_dir2(const TC &tc)
{
    canfd_frame cf; memset(&cf, 0, sizeof cf);
    cf.can_id = tc.can_id; cf.len = tc.len;
    if (tc.data) memcpy(cf.data, tc.data, tc.len);

    std::list<canfd_frame*> frames; frames.push_back(&cf);
    uint8_t pkt[256];
    uint8_t *endp = buildPacket(sizeof pkt, pkt, frames, 3,
        [](std::list<canfd_frame*>&, std::list<canfd_frame*>::iterator){});
    size_t n = (size_t)(endp - pkt);

    CapCtx c; memset(&c, 0, sizeof c);
    int got = cw_parse_udp(pkt, n, cap_cb, &c);
    CHECK(got == 1 && c.n == 1, "dir2: we parsed exactly 1 frame");
    CHECK(frames_equal(tc.can_id, tc.len, tc.data, c.f.can_id, c.f.len, c.f.data), tc.name);
}

/* ---------- direction 3: our encode_frame -> cannelloni decodeFrame -------- */
static void run_dir3(const TC &tc)
{
    cw_frame_t f; memset(&f, 0, sizeof f);
    f.can_id = tc.can_id; f.len = tc.len; f.fd = 0;
    if (tc.data) memcpy(f.data, tc.data, tc.len);
    uint8_t buf[128];
    size_t total = cw_encode_frame(buf, &f);

    /* drive cannelloni's streaming decoder exactly as tcpthread.cpp does */
    canfd_frame out; memset(&out, 0, sizeof out);
    DecodeState st = STATE_INIT;
    size_t off = 0;
    ssize_t want = decodeFrame(NULL, 0, &out, &st);   /* INIT -> asks for CAN_ID size */
    while (want > 0) {
        if (off + (size_t)want > total) { g_fail++; printf("  FAIL: dir3 overrun\n"); break; }
        ssize_t next = decodeFrame(buf + off, (size_t)want, &out, &st);
        off += (size_t)want;
        want = next;
    }
    CHECK(want == 0, "dir3: decode completed");
    CHECK(frames_equal(tc.can_id, tc.len, tc.data,
                       out.can_id, (uint8_t)(out.len & 0x7F), out.data), tc.name);
}

/* ---------- direction 4: cannelloni encodeFrame -> our cw_decode_stream ---- */
static void run_dir4(const TC &tc)
{
    canfd_frame cf; memset(&cf, 0, sizeof cf);
    cf.can_id = tc.can_id; cf.len = tc.len;
    if (tc.data) memcpy(cf.data, tc.data, tc.len);
    uint8_t buf[128];
    size_t total = encodeFrame(buf, &cf);

    cw_frame_t out; memset(&out, 0, sizeof out);
    cw_decode_state_t st = CW_ST_INIT;
    size_t off = 0;
    CW_SSIZE want = cw_decode_stream(NULL, 0, &out, &st);   /* INIT -> CAN_ID size */
    while (want > 0) {
        if (off + (size_t)want > total) { g_fail++; printf("  FAIL: dir4 overrun\n"); break; }
        CW_SSIZE next = cw_decode_stream(buf + off, (size_t)want, &out, &st);
        off += (size_t)want;
        want = next;
    }
    CHECK(want == 0, "dir4: decode completed");
    CHECK(frames_equal(tc.can_id, tc.len, tc.data, out.can_id, out.len, out.data), tc.name);
}

int main(void)
{
    static const uint8_t d8[]  = {1,2,3,4,5,6,7,8};
    static const uint8_t d3[]  = {0xAA,0xBB,0xCC};
    TC cases[] = {
        { "STD id=0x123 dlc=8", 0x123,                         8, d8 },
        { "STD id=0x000 dlc=0", 0x000,                         0, NULL },
        { "STD id=0x7FF dlc=3", 0x7FF,                         3, d3 },
        { "EXT id=0x1ABCDEF8",  0x1ABCDEF8u | CW_CAN_EFF_FLAG, 8, d8 },
        { "EXT RTR dlc=4",      0x18FF0000u | CW_CAN_EFF_FLAG | CW_CAN_RTR_FLAG, 4, NULL },
    };
    int nc = (int)(sizeof cases / sizeof cases[0]);

    printf("dir1: our UDP encode -> cannelloni parseFrames\n");
    for (int i=0;i<nc;i++) run_dir1(cases[i]);
    printf("dir2: cannelloni buildPacket -> our cw_parse_udp\n");
    for (int i=0;i<nc;i++) run_dir2(cases[i]);
    printf("dir3: our cw_encode_frame -> cannelloni decodeFrame\n");
    for (int i=0;i<nc;i++) run_dir3(cases[i]);
    printf("dir4: cannelloni encodeFrame -> our cw_decode_stream\n");
    for (int i=0;i<nc;i++) run_dir4(cases[i]);

    /* round-trip the Kvaser id translation */
    {
        long id; unsigned int fl;
        uint32_t cid = cw_kvaser_to_canid(0x1ABCDEF8, CW_canMSG_EXT);
        cw_canid_to_kvaser(cid, 0, &id, &fl);
        CHECK(id == 0x1ABCDEF8 && (fl & CW_canMSG_EXT), "kvaser EXT round-trip");
        cid = cw_kvaser_to_canid(0x123, CW_canMSG_STD);
        cw_canid_to_kvaser(cid, 0, &id, &fl);
        CHECK(id == 0x123 && (fl & CW_canMSG_STD), "kvaser STD round-trip");
    }

    printf(g_fail ? "\nRESULT: FAIL (%d)\n" : "\nRESULT: PASS\n", g_fail);
    return g_fail ? 1 : 0;
}
