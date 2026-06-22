/*
 * cannelloni_wire.c — see cannelloni_wire.h. Mirrors refs/cannelloni/parser.cpp
 * (UDP) and refs/cannelloni/decoder.cpp (TCP streaming) so the shim is a
 * byte-compatible cannelloni peer.
 */
#include "cannelloni_wire.h"
#include <string.h>

/* ===================== Kvaser <-> SocketCAN translation ===================== */

uint32_t cw_kvaser_to_canid(long id, unsigned int kvaser_flag)
{
    uint32_t can_id;
    if (kvaser_flag & CW_canMSG_EXT)
        can_id = ((uint32_t)id & CW_CAN_EFF_MASK) | CW_CAN_EFF_FLAG;
    else
        can_id = (uint32_t)id & CW_CAN_SFF_MASK;
    if (kvaser_flag & CW_canMSG_RTR)
        can_id |= CW_CAN_RTR_FLAG;
    return can_id;
}

void cw_canid_to_kvaser(uint32_t can_id, int fd,
                        long *out_id, unsigned int *out_flag)
{
    unsigned int flag;
    if (can_id & CW_CAN_EFF_FLAG) {
        if (out_id) *out_id = (long)(can_id & CW_CAN_EFF_MASK);
        flag = CW_canMSG_EXT;
    } else {
        if (out_id) *out_id = (long)(can_id & CW_CAN_SFF_MASK);
        flag = CW_canMSG_STD;
    }
    if (can_id & CW_CAN_RTR_FLAG)
        flag |= CW_canMSG_RTR;
    if (fd)
        flag |= CW_canMSG_FDF;
    if (out_flag) *out_flag = flag;
}

/* ============================ per-frame codec ============================== */

size_t cw_encode_frame(uint8_t *out, const cw_frame_t *f)
{
    uint8_t *p = out;
    uint32_t tmp = htonl(f->can_id);
    uint8_t len = (uint8_t)(f->len & 0x7F);
    if (f->fd) len |= CW_CANFD_FRAME;

    memcpy(p, &tmp, sizeof(tmp));
    p += sizeof(tmp);
    *p++ = len;
    if (f->fd)
        *p++ = f->fd_flags;
    if ((f->can_id & CW_CAN_RTR_FLAG) == 0) {
        uint8_t dlen = (uint8_t)(f->len & 0x7F);
        if (dlen) {
            memcpy(p, f->data, dlen);
            p += dlen;
        }
    }
    return (size_t)(p - out);
}

CW_SSIZE cw_decode_stream(const uint8_t *data, size_t len,
                          cw_frame_t *f, cw_decode_state_t *state)
{
    switch (*state) {
    case CW_ST_INIT:
        f->fd = 0; f->fd_flags = 0; f->len = 0;
        *state = CW_ST_CAN_ID;
        return 4;
    case CW_ST_CAN_ID: {
        uint32_t tmp;
        if (len != 4) return -1;
        memcpy(&tmp, data, sizeof(tmp));
        f->can_id = ntohl(tmp);
        *state = CW_ST_LEN;
        return 1;
    }
    case CW_ST_LEN: {
        uint8_t raw;
        if (len != 1) return -1;
        raw = data[0];
        if (raw & CW_CANFD_FRAME) {
            f->fd = 1;
            f->len = (uint8_t)(raw & ~CW_CANFD_FRAME);
            *state = CW_ST_FLAGS;
            return 1;
        }
        f->fd = 0;
        f->len = raw;
        if (f->can_id & CW_CAN_RTR_FLAG) { *state = CW_ST_INIT; f->len = 0; return 0; }
        if (f->len == 0) { *state = CW_ST_INIT; return 0; }
        *state = CW_ST_DATA;
        return f->len;
    }
    case CW_ST_FLAGS:
        if (len != 1) return -1;
        f->fd_flags = data[0];
        if (f->can_id & CW_CAN_RTR_FLAG) { *state = CW_ST_INIT; return 0; }
        if (f->len == 0) { *state = CW_ST_INIT; return 0; }
        *state = CW_ST_DATA;
        return f->len;
    case CW_ST_DATA:
        if (len != f->len) return -1;
        memcpy(f->data, data, f->len);
        *state = CW_ST_INIT;
        return 0;
    }
    return -1;
}

/* ============================ UDP packet codec ============================= */

size_t cw_build_udp(uint8_t *out, const cw_frame_t *f, uint8_t seq_no)
{
    uint16_t count = htons(1);
    out[0] = CW_FRAME_VERSION;
    out[1] = CW_OP_DATA;
    out[2] = seq_no;
    memcpy(&out[3], &count, sizeof(count));
    return CW_DATA_PACKET_BASE_SIZE + cw_encode_frame(out + CW_DATA_PACKET_BASE_SIZE, f);
}

/* Parse one frame at *pp (advancing it); mirrors parser.cpp::parseCANFrame.
 * Returns 0 on success, -1 on truncation. */
static int parse_one(const uint8_t **pp, const uint8_t *end, cw_frame_t *f)
{
    const uint8_t *p = *pp;
    uint32_t tmp;
    uint8_t raw, dlen;

    if (p + CW_FRAME_BASE_SIZE > end) return -1;
    memcpy(&tmp, p, sizeof(tmp)); p += sizeof(tmp);
    f->can_id = ntohl(tmp);
    raw = *p++;
    if (raw & CW_CANFD_FRAME) {
        f->fd = 1;
        f->len = (uint8_t)(raw & ~CW_CANFD_FRAME);
        if (p + 1 > end) return -1;
        f->fd_flags = *p++;
    } else {
        f->fd = 0;
        f->fd_flags = 0;
        f->len = raw;
    }
    dlen = (uint8_t)(f->len & 0x7F);
    if ((f->can_id & CW_CAN_RTR_FLAG) == 0) {
        if (p + dlen > end) return -1;
        if (dlen) memcpy(f->data, p, dlen);
        p += dlen;
    }
    *pp = p;
    return 0;
}

int cw_parse_udp(const uint8_t *buf, size_t len, cw_frame_cb cb, void *user)
{
    const uint8_t *p, *end;
    uint16_t count, i;
    int delivered = 0;

    if (len < CW_DATA_PACKET_BASE_SIZE) return -1;
    if (buf[0] != CW_FRAME_VERSION) return -1;
    if (buf[1] != CW_OP_DATA) return -1;
    memcpy(&count, &buf[3], sizeof(count));
    count = ntohs(count);

    p = buf + CW_DATA_PACKET_BASE_SIZE;
    end = buf + len;
    for (i = 0; i < count; i++) {
        cw_frame_t f;
        memset(&f, 0, sizeof(f));
        if (parse_one(&p, end, &f) != 0) return -1;
        if (cb) cb(user, &f);
        delivered++;
    }
    return delivered;
}
