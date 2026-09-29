// One RTMP connection per process. stdin/stdout carry length-prefixed AMF0
// command bodies; librtmp owns the handshake, encryption and control packets.
#include <librtmp/rtmp.h>
#include <librtmp/log.h>
#include <arpa/inet.h>
#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/select.h>
#include <unistd.h>

#define MAX_FRAME (8 * 1024 * 1024)

static void quiet_log(int level, const char *format, va_list arguments) {
    (void)level; (void)format; (void)arguments;
}

static int transfer(int fd, void *buffer, size_t length, int writing) {
    unsigned char *p = buffer;
    while (length) {
        ssize_t n = writing ? write(fd, p, length) : read(fd, p, length);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) return 0;
        p += n;
        length -= (size_t)n;
    }
    return 1;
}

static int read_command(RTMPPacket *packet) {
    uint32_t length;
    if (!transfer(STDIN_FILENO, &length, 4, 0)) return 0;
    length = ntohl(length);
    if (!length || length > MAX_FRAME) return 0;
    RTMPPacket_Reset(packet);
    if (!RTMPPacket_Alloc(packet, (int)length)) return 0;
    packet->m_packetType = RTMP_PACKET_TYPE_INVOKE;
    packet->m_nChannel = 3;
    packet->m_headerType = RTMP_PACKET_SIZE_LARGE;
    packet->m_nBodySize = length;
    if (!transfer(STDIN_FILENO, packet->m_body, length, 0)) {
        RTMPPacket_Free(packet);
        return 0;
    }
    return 1;
}

static int write_command(const char *body, uint32_t length) {
    uint32_t header = htonl(length);
    return length <= MAX_FRAME && transfer(STDOUT_FILENO, &header, 4, 1) &&
        transfer(STDOUT_FILENO, (void *)body, length, 1);
}

int main(int argc, char **argv) {
    if (argc != 2) return 2;
    signal(SIGPIPE, SIG_IGN);
    // Library diagnostics can contain AMF arguments. Never log them.
    RTMP_LogSetCallback(quiet_log);
    RTMP *rtmp = RTMP_Alloc();
    if (!rtmp) return 2;
    RTMP_Init(rtmp);
    char *url = strdup(argv[1]);
    if (!url || !RTMP_SetupURL(rtmp, url)) return 2;
    char *authority = strstr(url, "://");
    char *path = authority ? strchr(authority + 3, '/') : NULL;
    if (!path) return 2;
    AVal app_option = AVC("app");
    AVal app = { path + 1, (int)strlen(path + 1) };
    while (app.av_len > 0 && app.av_val[app.av_len - 1] == '/') app.av_len--;
    RTMP_SetOpt(rtmp, &app_option, &app);
    rtmp->Link.timeout = 15;
    RTMPPacket command = {0};
    if (!read_command(&command)) return 2;
    fprintf(stderr, "Starting RTMP handshake\n");
    int connected = RTMP_Connect(rtmp, &command);
    RTMPPacket_Free(&command);
    if (!connected) {
        fprintf(stderr, "RTMP handshake failed\n");
        RTMP_Close(rtmp); RTMP_Free(rtmp); free(url);
        return 1;
    }
    fprintf(stderr, "RTMP handshake complete\n");
    RTMPPacket packet = {0};
    int okay = 1;
    while (okay && RTMP_IsConnected(rtmp)) {
        int socket = RTMP_Socket(rtmp);
        fd_set read_set;
        FD_ZERO(&read_set);
        FD_SET(STDIN_FILENO, &read_set);
        FD_SET(socket, &read_set);
        struct timeval immediate = {0, 0};
        int buffered = rtmp->m_sb.sb_size > 0;
        int ready = select(socket + 1, &read_set, NULL, NULL, buffered ? &immediate : NULL);
        if (ready < 0) { if (errno == EINTR) continue; break; }
        if (FD_ISSET(STDIN_FILENO, &read_set)) {
            if (!read_command(&command)) break;
            okay = RTMP_SendPacket(rtmp, &command, 0);
            RTMPPacket_Free(&command);
        }
        if (okay && (buffered || FD_ISSET(socket, &read_set))) {
            if (!RTMP_ReadPacket(rtmp, &packet)) break;
            if (!RTMPPacket_IsReady(&packet)) continue;
            if (packet.m_packetType == RTMP_PACKET_TYPE_INVOKE) {
                okay = write_command(packet.m_body, packet.m_nBodySize);
            } else if (packet.m_packetType == RTMP_PACKET_TYPE_FLEX_MESSAGE &&
                       packet.m_nBodySize > 1 && packet.m_body[0] == 0) {
                okay = write_command(packet.m_body + 1, packet.m_nBodySize - 1);
            } else {
                // Ping/pong, window acknowledgements and chunk sizes. Do not
                // route INVOKE through librtmp's media-player state machine.
                RTMP_ClientPacket(rtmp, &packet);
            }
            RTMPPacket_Free(&packet);
            RTMPPacket_Reset(&packet);
        }
    }
    RTMPPacket_Free(&packet);
    RTMP_Close(rtmp); RTMP_Free(rtmp); free(url);
    return 0;
}
