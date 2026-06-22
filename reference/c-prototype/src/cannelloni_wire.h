/*
 * cannelloni_wire.h — cannelloni wire-format codec + Kvaser<->SocketCAN id
 * translation, shared by the canlib32.dll shim and the host unit test.
 *
 * Wire format reference: refs/cannelloni/parser.cpp (UDP, packet-framed) and
 * refs/cannelloni/decoder.cpp (TCP, headerless streaming). This file mirrors
 * those byte-for-byte so the shim interoperates with a stock cannelloni peer.
 *
 * Pure C, no OS dependency beyond htonl/ntohl (pulled from <winsock2.h> under
 * mingw, <arpa/inet.h> on a host build) so the very same object can be linked
 * into the Windows DLL and the native Linux test.
 */
#ifndef CANNELLONI_WIRE_H
#define CANNELLONI_WIRE_H

#include <stddef.h>
#include <stdint.h>

#if defined(_WIN32)
#  include <winsock2.h>            /* htonl/htons/ntohl/ntohs */
typedef long ssize_t_compat;       /* avoid clashing with mingw's ssize_t */
#  define CW_SSIZE ssize_t_compat
#else
#  include <arpa/inet.h>
#  include <sys/types.h>           /* ssize_t */
#  define CW_SSIZE ssize_t
#endif

#ifdef __cplusplus
extern "C" {
#endif

/* ---- cannelloni protocol constants (refs/cannelloni/cannelloni.h, decoder.h) ---- */
#define CW_FRAME_VERSION          2      /* CANNELLONI_FRAME_VERSION */
#define CW_OP_DATA                0      /* op_codes::DATA */
#define CW_OP_ACK                 1
#define CW_OP_NACK                2
#define CW_DATA_PACKET_BASE_SIZE  5      /* version+op+seq+count(2) */
#define CW_FRAME_BASE_SIZE        5      /* can_id(4)+len(1) */
#define CW_CANFD_FRAME            0x80   /* high bit of len => CAN FD frame */
#define CW_CONNECT_V1_STRING      "CANNELLONIv1"   /* TCP handshake (tcpthread.h) */
#define CW_CONNECT_V1_LEN         12     /* strlen, no NUL on the wire */
/* Max single classic/FD encoded frame: id(4)+len(1)+flags(1)+data(64) */
#define CW_MAX_FRAME_BYTES        70

/* ---- SocketCAN can_id flag bits (linux/can.h) ---- */
#define CW_CAN_EFF_FLAG  0x80000000u    /* extended (29-bit) frame */
#define CW_CAN_RTR_FLAG  0x40000000u    /* remote transmission request */
#define CW_CAN_ERR_FLAG  0x20000000u    /* error frame */
#define CW_CAN_EFF_MASK  0x1FFFFFFFu
#define CW_CAN_SFF_MASK  0x000007FFu

/* ---- Kvaser canMSG_* message flags (refs/kvaser_canlib/.../canlib.h) ---- */
#define CW_canMSG_RTR    0x0001
#define CW_canMSG_STD    0x0002
#define CW_canMSG_EXT    0x0004
#define CW_canMSG_FDF    0x010000        /* CAN FD frame */
#define CW_canMSG_BRS    0x020000        /* CAN FD bit-rate switch */

/* A decoded CAN frame in SocketCAN terms (can_id carries the flag bits). */
typedef struct {
    uint32_t can_id;     /* incl. EFF/RTR flag bits */
    uint8_t  len;        /* payload length, 0..8 (classic) or 0..64 (FD) */
    uint8_t  fd;         /* 1 if CAN FD frame */
    uint8_t  fd_flags;   /* cannelloni FD flags byte (valid when fd) */
    uint8_t  data[64];
} cw_frame_t;

/* ===================== Kvaser <-> SocketCAN translation ===================== */

/* Build a SocketCAN can_id from a Kvaser id + canMSG_* flags. */
uint32_t cw_kvaser_to_canid(long id, unsigned int kvaser_flag);

/* Split a SocketCAN can_id into a Kvaser id and canMSG_* flags. */
void cw_canid_to_kvaser(uint32_t can_id, int fd,
                        long *out_id, unsigned int *out_flag);

/* ============================ per-frame codec ============================== */

/* Encode one frame (cannelloni encodeFrame). Returns bytes written into out
 * (out must hold >= CW_MAX_FRAME_BYTES). */
size_t cw_encode_frame(uint8_t *out, const cw_frame_t *f);

/* Streaming TCP decoder, mirroring cannelloni decoder.cpp::decodeFrame.
 * Drive it the same way cannelloni does: start with state=0 (STATE_INIT),
 * repeatedly call with exactly the number of bytes it last asked for.
 * Returns the number of bytes to read next, 0 when a frame is complete in *f,
 * or -1 on protocol error. */
typedef enum {
    CW_ST_INIT = 0, CW_ST_CAN_ID, CW_ST_LEN, CW_ST_FLAGS, CW_ST_DATA
} cw_decode_state_t;

CW_SSIZE cw_decode_stream(const uint8_t *data, size_t len,
                          cw_frame_t *f, cw_decode_state_t *state);

/* ============================ UDP packet codec ============================= */

/* Build a one-frame cannelloni UDP datagram (version/op=DATA/seq/count=1 + frame).
 * Returns total datagram length. out must hold >= CW_DATA_PACKET_BASE_SIZE +
 * CW_MAX_FRAME_BYTES. */
size_t cw_build_udp(uint8_t *out, const cw_frame_t *f, uint8_t seq_no);

/* Parse a cannelloni UDP datagram, invoking cb(user, frame) for each contained
 * frame. Returns number of frames delivered, or -1 on malformed packet. */
typedef void (*cw_frame_cb)(void *user, const cw_frame_t *f);
int cw_parse_udp(const uint8_t *buf, size_t len, cw_frame_cb cb, void *user);

#ifdef __cplusplus
}
#endif
#endif /* CANNELLONI_WIRE_H */
