/*
 * canlib32.dll SHIM — Kvaser CANlib API -> cannelloni (UDP or TCP) -> Linux vcan.
 *
 * Drop-in replacement for Kvaser's canlib32.dll that implements exactly the 13
 * symbols the target Windows app resolves (see refs/canlib32.dll export table).
 * Rather than touching hardware, the shim is itself a cannelloni peer: it speaks
 * cannelloni's wire format to a stock `cannelloni -I vcan0 ...` on a Linux host.
 *
 *   Windows app -> canlib32.dll (this) --UDP|TCP--> cannelloni -> vcan0 -> Linux CAN
 *
 * Wire codec lives in cannelloni_wire.c (shared with the host test).
 *
 * Config via environment (passed through by the launcher / set system-wide):
 *   CANSHIM_HOST      Linux cannelloni IP            (default 127.0.0.1)
 *   CANSHIM_PORT      remote port to send to         (default 20000)
 *   CANSHIM_LOCALPORT UDP bind / TCP server port     (default 20000)
 *   CANSHIM_PROTO     "udp" | "tcp"                  (default "udp")
 *   CANSHIM_TCPROLE   "client" | "server"            (default "client")
 *   CANSHIM_LOG       path; if set, append debug log
 *
 * cannelloni on the Linux side:
 *   UDP        : cannelloni -I vcan0 -R <win-ip> -r <CANSHIM_LOCALPORT> -l <CANSHIM_PORT>
 *   TCP (shim=client): cannelloni -C s -I vcan0 -l <CANSHIM_PORT>
 *   TCP (shim=server): cannelloni -C c -I vcan0 -R <win-ip> -r <CANSHIM_LOCALPORT>
 *
 * Build (see Makefile):
 *   i686-w64-mingw32-gcc -shared -O2 canlib32_shim.c cannelloni_wire.c \
 *       -o canlib32.dll canlib32.def -Wl,--kill-at -lws2_32
 */
#include <winsock2.h>
#include <ws2tcpip.h>
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <stdarg.h>

#include "cannelloni_wire.h"

/* ---- CANlib return codes / message flags (refs/kvaser_canlib/canstat.h) ---- */
#define canOK            0
#define canERR_PARAM   (-1)
#define canERR_NOMSG   (-2)
#define canERR_NOTFOUND (-3)

typedef int canHandle;
typedef int canStatus;

/* ---- transport state (the tool opens a single channel) ---- */
static int      g_proto      = 0;          /* 0=udp 1=tcp */
static int      g_tcp_server = 0;
static SOCKET   g_sock       = INVALID_SOCKET;   /* active data socket */
static SOCKET   g_listen     = INVALID_SOCKET;   /* tcp server listen socket */
static struct sockaddr_in g_remote;              /* udp send target */
static volatile int g_running    = 0;
static volatile int g_negotiated = 0;            /* tcp handshake complete */
static uint8_t  g_seq = 0;

static CRITICAL_SECTION g_tx_lock, g_rx_lock;
static HANDLE g_rx_thread = NULL;

/* RX ring of decoded frames, filled by the reader thread */
#define RXCAP 8192
static cw_frame_t g_rx[RXCAP];
static volatile unsigned g_rx_head = 0, g_rx_tail = 0;   /* head=write tail=read */

static void dbg(const char *fmt, ...)
{
    const char *p = getenv("CANSHIM_LOG");
    FILE *f;
    va_list ap;
    if (!p) return;
    f = fopen(p, "a");
    if (!f) return;
    va_start(ap, fmt);
    vfprintf(f, fmt, ap);
    va_end(ap);
    fputc('\n', f);
    fclose(f);
}

static void ring_push(const cw_frame_t *f)
{
    unsigned next;
    EnterCriticalSection(&g_rx_lock);
    next = (g_rx_head + 1) % RXCAP;
    if (next != g_rx_tail) { g_rx[g_rx_head] = *f; g_rx_head = next; }
    /* else ring full -> drop oldest-newest policy: silently drop */
    LeaveCriticalSection(&g_rx_lock);
}

static void rx_cb(void *user, const cw_frame_t *f) { (void)user; ring_push(f); }

static int recv_all(SOCKET s, char *buf, int n)
{
    int got = 0;
    while (got < n) {
        int r = recv(s, buf + got, n - got, 0);
        if (r <= 0) return -1;
        got += r;
    }
    return 0;
}

/* ----------------------------- RX threads ----------------------------- */

static DWORD WINAPI rx_thread_udp(LPVOID arg)
{
    uint8_t buf[2048];
    (void)arg;
    while (g_running) {
        struct sockaddr_in from;
        int flen = sizeof from;
        int r = recvfrom(g_sock, (char*)buf, sizeof buf, 0,
                         (struct sockaddr*)&from, &flen);
        if (r <= 0) { if (g_running) dbg("udp rx error %d", WSAGetLastError()); break; }
        if (cw_parse_udp(buf, (size_t)r, rx_cb, NULL) < 0)
            dbg("udp: malformed packet (%d bytes)", r);
    }
    return 0;
}

/* one TCP handshake: send + expect "CANNELLONIv1" */
static int tcp_handshake(SOCKET s)
{
    char hs[CW_CONNECT_V1_LEN];
    if (send(s, CW_CONNECT_V1_STRING, CW_CONNECT_V1_LEN, 0) != CW_CONNECT_V1_LEN) return -1;
    if (recv_all(s, hs, CW_CONNECT_V1_LEN) != 0) return -1;
    if (memcmp(hs, CW_CONNECT_V1_STRING, CW_CONNECT_V1_LEN) != 0) return -1;
    return 0;
}

static DWORD WINAPI rx_thread_tcp(LPVOID arg)
{
    (void)arg;
    while (g_running) {
        cw_frame_t f;
        cw_decode_state_t st = CW_ST_INIT;
        CW_SSIZE want;

        if (g_tcp_server) {
            struct sockaddr_in ca; int cl = sizeof ca;
            SOCKET c = accept(g_listen, (struct sockaddr*)&ca, &cl);
            if (c == INVALID_SOCKET) { if (g_running) dbg("accept failed %d", WSAGetLastError()); break; }
            EnterCriticalSection(&g_tx_lock);
            g_sock = c;
            LeaveCriticalSection(&g_tx_lock);
        }
        /* g_sock is the connected socket (server: just accepted, client: connect()) */
        if (g_sock == INVALID_SOCKET) break;
        { int one = 1; setsockopt(g_sock, IPPROTO_TCP, TCP_NODELAY, (char*)&one, sizeof one); }
        if (tcp_handshake(g_sock) != 0) {
            dbg("tcp handshake failed");
            EnterCriticalSection(&g_tx_lock);
            closesocket(g_sock); g_sock = INVALID_SOCKET; g_negotiated = 0;
            LeaveCriticalSection(&g_tx_lock);
            if (g_tcp_server && g_running) { Sleep(200); continue; }
            break;
        }
        g_negotiated = 1;
        dbg("tcp negotiated");

        /* stream-decode frames, mirroring cannelloni's read loop */
        memset(&f, 0, sizeof f);
        want = cw_decode_stream(NULL, 0, &f, &st);   /* INIT -> CAN_ID size */
        while (g_running && want > 0) {
            char chunk[80];
            if (want > (CW_SSIZE)sizeof chunk) { dbg("tcp: oversize want %ld", (long)want); break; }
            if (recv_all(g_sock, chunk, (int)want) != 0) break;
            want = cw_decode_stream((uint8_t*)chunk, (size_t)want, &f, &st);
            if (want == 0) {            /* frame complete */
                ring_push(&f);
                memset(&f, 0, sizeof f);
                st = CW_ST_INIT;
                want = cw_decode_stream(NULL, 0, &f, &st);
            } else if (want < 0) {
                dbg("tcp decoder error");
                break;
            }
        }
        g_negotiated = 0;
        EnterCriticalSection(&g_tx_lock);
        if (g_sock != INVALID_SOCKET) { closesocket(g_sock); g_sock = INVALID_SOCKET; }
        LeaveCriticalSection(&g_tx_lock);
        if (!g_tcp_server) break;       /* client: one connection then stop */
    }
    return 0;
}

/* --------------------------- connection setup --------------------------- */

static unsigned short env_port(const char *name, unsigned short dflt)
{
    const char *s = getenv(name);
    return s ? (unsigned short)atoi(s) : dflt;
}

static int connect_transport(void)
{
    const char *host, *proto, *role;
    unsigned short rport, lport;

    if (g_sock != INVALID_SOCKET || g_listen != INVALID_SOCKET) return canOK;

    host  = getenv("CANSHIM_HOST");  if (!host)  host  = "127.0.0.1";
    proto = getenv("CANSHIM_PROTO"); if (!proto) proto = "udp";
    role  = getenv("CANSHIM_TCPROLE");
    rport = env_port("CANSHIM_PORT", 20000);
    lport = env_port("CANSHIM_LOCALPORT", 20000);
    g_proto = (proto[0] == 't' || proto[0] == 'T') ? 1 : 0;
    g_tcp_server = (role && (role[0] == 's' || role[0] == 'S')) ? 1 : 0;

    memset(&g_remote, 0, sizeof g_remote);
    g_remote.sin_family = AF_INET;
    g_remote.sin_port   = htons(rport);
    g_remote.sin_addr.s_addr = inet_addr(host);

    if (g_proto == 0) {                         /* ---- UDP ---- */
        struct sockaddr_in la;
        SOCKET s = socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP);
        if (s == INVALID_SOCKET) { dbg("udp socket failed %d", WSAGetLastError()); return canERR_PARAM; }
        memset(&la, 0, sizeof la);
        la.sin_family = AF_INET; la.sin_port = htons(lport);
        la.sin_addr.s_addr = htonl(INADDR_ANY);
        if (bind(s, (struct sockaddr*)&la, sizeof la) != 0) {
            dbg("udp bind :%u failed %d", lport, WSAGetLastError());
            closesocket(s); return canERR_PARAM;
        }
        g_sock = s; g_negotiated = 1;
        g_running = 1; g_rx_head = g_rx_tail = 0;
        g_rx_thread = CreateThread(NULL, 0, rx_thread_udp, NULL, 0, NULL);
        dbg("udp ready: bind :%u, remote %s:%u", lport, host, rport);
        return canOK;
    }

    /* ---- TCP ---- */
    g_running = 1; g_rx_head = g_rx_tail = 0; g_negotiated = 0;
    if (g_tcp_server) {
        struct sockaddr_in la;
        int opt = 1;
        SOCKET ls = socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
        if (ls == INVALID_SOCKET) { dbg("tcp socket failed %d", WSAGetLastError()); return canERR_PARAM; }
        setsockopt(ls, SOL_SOCKET, SO_REUSEADDR, (char*)&opt, sizeof opt);
        memset(&la, 0, sizeof la);
        la.sin_family = AF_INET; la.sin_port = htons(lport);
        la.sin_addr.s_addr = htonl(INADDR_ANY);
        if (bind(ls, (struct sockaddr*)&la, sizeof la) != 0 || listen(ls, 1) != 0) {
            dbg("tcp bind/listen :%u failed %d", lport, WSAGetLastError());
            closesocket(ls); return canERR_PARAM;
        }
        g_listen = ls;
        dbg("tcp server: listening :%u", lport);
    } else {
        SOCKET s = socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
        if (s == INVALID_SOCKET) { dbg("tcp socket failed %d", WSAGetLastError()); return canERR_PARAM; }
        if (connect(s, (struct sockaddr*)&g_remote, sizeof g_remote) != 0) {
            dbg("tcp connect %s:%u failed %d", host, rport, WSAGetLastError());
            closesocket(s); return canERR_PARAM;
        }
        g_sock = s;
        dbg("tcp client: connected %s:%u", host, rport);
    }
    g_rx_thread = CreateThread(NULL, 0, rx_thread_tcp, NULL, 0, NULL);
    return canOK;
}

/* ============================ exported CANlib API ============================ */

__declspec(dllexport) void __stdcall canInitializeLibrary(void) { dbg("canInitializeLibrary"); }

__declspec(dllexport) canHandle __stdcall canOpenChannel(int channel, int flags)
{
    dbg("canOpenChannel(ch=%d flags=0x%x)", channel, flags);
    if (connect_transport() != canOK) return canERR_NOTFOUND;
    return 1;   /* fixed non-negative handle */
}

__declspec(dllexport) canStatus __stdcall canSetBusParams(canHandle h, long freq,
        unsigned int t1, unsigned int t2, unsigned int sjw, unsigned int ns, unsigned int sm)
{
    (void)h;(void)t1;(void)t2;(void)sjw;(void)ns;(void)sm;
    dbg("canSetBusParams(freq=%ld)", freq);
    return canOK;
}

__declspec(dllexport) canStatus __stdcall canBusOn(canHandle h)  { (void)h; dbg("canBusOn");  return canOK; }
__declspec(dllexport) canStatus __stdcall canBusOff(canHandle h) { (void)h; dbg("canBusOff"); return canOK; }

__declspec(dllexport) canStatus __stdcall canWrite(canHandle h, long id, void *msg,
        unsigned int dlc, unsigned int flag)
{
    cw_frame_t f;
    int r;
    (void)h;
    if (!g_negotiated || g_sock == INVALID_SOCKET) return canERR_PARAM;

    memset(&f, 0, sizeof f);
    f.can_id = cw_kvaser_to_canid(id, flag);
    f.len = (uint8_t)(dlc > 8 ? 8 : dlc);     /* classic CAN */
    if (msg && f.len && !(flag & CW_canMSG_RTR)) memcpy(f.data, msg, f.len);

    EnterCriticalSection(&g_tx_lock);
    if (g_proto == 0) {
        uint8_t pkt[CW_DATA_PACKET_BASE_SIZE + CW_MAX_FRAME_BYTES];
        size_t n = cw_build_udp(pkt, &f, g_seq++);
        r = sendto(g_sock, (char*)pkt, (int)n, 0,
                   (struct sockaddr*)&g_remote, sizeof g_remote);
        r = (r == (int)n) ? 0 : -1;
    } else {
        uint8_t enc[CW_MAX_FRAME_BYTES];
        size_t n = cw_encode_frame(enc, &f);
        r = (send(g_sock, (char*)enc, (int)n, 0) == (int)n) ? 0 : -1;
    }
    LeaveCriticalSection(&g_tx_lock);

    dbg("canWrite id=0x%lx dlc=%u flag=0x%x -> %s", id, dlc, flag, r == 0 ? "ok" : "ERR");
    return (r == 0) ? canOK : canERR_PARAM;
}

__declspec(dllexport) canStatus __stdcall canRead(canHandle h, long *id, void *msg,
        unsigned int *dlc, unsigned int *flag, unsigned long *time)
{
    cw_frame_t f;
    int have = 0;
    long oid; unsigned int oflag;
    (void)h;

    EnterCriticalSection(&g_rx_lock);
    if (g_rx_tail != g_rx_head) { f = g_rx[g_rx_tail]; g_rx_tail = (g_rx_tail + 1) % RXCAP; have = 1; }
    LeaveCriticalSection(&g_rx_lock);
    if (!have) return canERR_NOMSG;

    cw_canid_to_kvaser(f.can_id, f.fd, &oid, &oflag);
    if (id)   *id   = oid;
    if (flag) *flag = oflag;
    if (dlc)  *dlc  = f.len;
    if (time) *time = (unsigned long)GetTickCount();
    if (msg && f.len && !(f.can_id & CW_CAN_RTR_FLAG))
        memcpy(msg, f.data, f.len);
    return canOK;
}

__declspec(dllexport) canStatus __stdcall canReadStatus(canHandle h, unsigned long *flags)
{ (void)h; if (flags) *flags = 0; return canOK; }

__declspec(dllexport) canStatus __stdcall canReadErrorCounters(canHandle h,
        unsigned int *tx, unsigned int *rx, unsigned int *ov)
{ (void)h; if(tx)*tx=0; if(rx)*rx=0; if(ov)*ov=0; return canOK; }

__declspec(dllexport) canStatus __stdcall canGetBusStatistics(canHandle h, void *stat, size_t n)
{ (void)h; if (stat && n) memset(stat, 0, n); return canOK; }

__declspec(dllexport) unsigned short __stdcall canGetVersion(void) { return 0x0900; /* 9.0 */ }

__declspec(dllexport) canStatus __stdcall canGetErrorText(canStatus err, char *buf, unsigned int n)
{
    const char *s;
    switch (err) {
    case canOK:          s = "OK"; break;
    case canERR_PARAM:   s = "Error in parameter"; break;
    case canERR_NOMSG:   s = "No messages available"; break;
    case canERR_NOTFOUND:s = "Specified device not found"; break;
    default:             s = "Unknown error"; break;
    }
    if (buf && n) { strncpy(buf, s, n - 1); buf[n - 1] = 0; }
    return canOK;
}

__declspec(dllexport) canStatus __stdcall canClose(canHandle h)
{
    (void)h;
    dbg("canClose");
    g_running = 0;
    g_negotiated = 0;
    if (g_sock != INVALID_SOCKET)   { shutdown(g_sock, SD_BOTH); closesocket(g_sock); g_sock = INVALID_SOCKET; }
    if (g_listen != INVALID_SOCKET) { closesocket(g_listen); g_listen = INVALID_SOCKET; }
    if (g_rx_thread) { WaitForSingleObject(g_rx_thread, 800); CloseHandle(g_rx_thread); g_rx_thread = NULL; }
    return canOK;
}

/* ---- DllMain: winsock + lock lifecycle ---- */
BOOL WINAPI DllMain(HINSTANCE inst, DWORD reason, LPVOID resv)
{
    (void)inst;(void)resv;
    if (reason == DLL_PROCESS_ATTACH) {
        WSADATA wsa;
        WSAStartup(MAKEWORD(2,2), &wsa);
        InitializeCriticalSection(&g_tx_lock);
        InitializeCriticalSection(&g_rx_lock);
    } else if (reason == DLL_PROCESS_DETACH) {
        canClose(1);
        DeleteCriticalSection(&g_tx_lock);
        DeleteCriticalSection(&g_rx_lock);
        WSACleanup();
    }
    return TRUE;
}
