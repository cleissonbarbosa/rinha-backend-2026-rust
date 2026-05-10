#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netdb.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/socket.h>
#include <sys/types.h>
#include <sys/un.h>
#include <unistd.h>

#define MAX_EVENTS 1024
#define MAX_FD 65536
#define BUF_SIZE 8192

typedef struct {
    int family;
    union {
        struct sockaddr_storage ip;
        struct sockaddr_un un;
    };
    socklen_t len;
} Backend;

typedef struct Conn {
    int fd;
    int peer;
    int pipe_r; // pipe used to splice(fd, peer)
    int pipe_w;
    int pipe_bytes; // bytes currently buffered in the pipe waiting for peer write
    int is_backend;
    int connected;
    int close_after_write;
    int splice_capable; // 1 if both fd and peer are kernel-spliceable
    size_t off;
    size_t len;
    unsigned char buf[BUF_SIZE];
} Conn;

static Backend backends[2];
static unsigned rr_cursor = 0;
static Conn *fdtab[MAX_FD];

static void resolve_backend_inet(int idx, const char *value) {
    char host[256];
    char port[32];
    const char *colon = strrchr(value, ':');
    if (!colon || colon == value || colon[1] == '\0') {
        fprintf(stderr, "invalid tcp backend: %s\n", value);
        exit(2);
    }
    size_t hlen = (size_t)(colon - value);
    if (hlen >= sizeof(host)) hlen = sizeof(host) - 1;
    memcpy(host, value, hlen);
    host[hlen] = '\0';
    snprintf(port, sizeof(port), "%s", colon + 1);

    struct addrinfo hints;
    struct addrinfo *res = NULL;
    memset(&hints, 0, sizeof(hints));
    hints.ai_socktype = SOCK_STREAM;
    hints.ai_family = AF_UNSPEC;
    int rc = getaddrinfo(host, port, &hints, &res);
    if (rc != 0 || !res) {
        fprintf(stderr, "getaddrinfo(%s): %s\n", value, gai_strerror(rc));
        exit(2);
    }
    backends[idx].family = res->ai_family;
    memcpy(&backends[idx].ip, res->ai_addr, res->ai_addrlen);
    backends[idx].len = (socklen_t)res->ai_addrlen;
    freeaddrinfo(res);
}

static void resolve_backend_unix(int idx, const char *path) {
    struct sockaddr_un *un = &backends[idx].un;
    memset(un, 0, sizeof(*un));
    un->sun_family = AF_UNIX;
    if (strlen(path) >= sizeof(un->sun_path)) {
        fprintf(stderr, "unix backend path too long: %s\n", path);
        exit(2);
    }
    strcpy(un->sun_path, path);
    backends[idx].family = AF_UNIX;
    backends[idx].len = (socklen_t)(offsetof(struct sockaddr_un, sun_path) + strlen(path) + 1);
}

static void resolve_backend(int idx, const char *value) {
    if (strncmp(value, "unix:", 5) == 0) {
        resolve_backend_unix(idx, value + 5);
    } else {
        resolve_backend_inet(idx, value);
    }
}

static int add_epoll(int ep, Conn *c, uint32_t extra) {
    struct epoll_event ev;
    memset(&ev, 0, sizeof(ev));
    ev.events = EPOLLIN | EPOLLRDHUP | extra;
    ev.data.fd = c->fd;
    return epoll_ctl(ep, EPOLL_CTL_ADD, c->fd, &ev);
}

static int mod_epoll(int ep, Conn *c) {
    struct epoll_event ev;
    memset(&ev, 0, sizeof(ev));
    ev.events = EPOLLIN | EPOLLRDHUP;
    if (c->len > c->off || (c->is_backend && !c->connected)) ev.events |= EPOLLOUT;
    ev.data.fd = c->fd;
    return epoll_ctl(ep, EPOLL_CTL_MOD, c->fd, &ev);
}

static void free_conn(int ep, int fd) {
    if (fd < 0 || fd >= MAX_FD || !fdtab[fd]) return;
    epoll_ctl(ep, EPOLL_CTL_DEL, fd, NULL);
    close(fd);
    free(fdtab[fd]);
    fdtab[fd] = NULL;
}

static void close_pair(int ep, int fd) {
    if (fd < 0 || fd >= MAX_FD || !fdtab[fd]) return;
    int peer = fdtab[fd]->peer;
    free_conn(ep, fd);
    free_conn(ep, peer);
}

static void enable_nodelay(int fd, int family) {
    if (family != AF_INET && family != AF_INET6) return;
    int one = 1;
    setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &one, sizeof(one));
#ifdef TCP_QUICKACK
    setsockopt(fd, IPPROTO_TCP, TCP_QUICKACK, &one, sizeof(one));
#endif
}

static int make_backend_socket(unsigned idx) {
    int family = backends[idx].family;
    int fd = socket(family, SOCK_STREAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0);
    if (fd < 0) return -1;
    enable_nodelay(fd, family);

    const struct sockaddr *sa;
    socklen_t slen;
    if (family == AF_UNIX) {
        sa = (const struct sockaddr *)&backends[idx].un;
        slen = backends[idx].len;
    } else {
        sa = (const struct sockaddr *)&backends[idx].ip;
        slen = backends[idx].len;
    }

    int rc = connect(fd, sa, slen);
    if (rc == 0) return fd;
    if (errno == EINPROGRESS) return fd;
    close(fd);
    return -1;
}

static Conn *new_conn(int fd, int peer, int backend, int connected) {
    if (fd < 0 || fd >= MAX_FD) return NULL;
    Conn *c = calloc(1, sizeof(Conn));
    if (!c) return NULL;
    c->fd = fd;
    c->peer = peer;
    c->is_backend = backend;
    c->connected = connected;
    fdtab[fd] = c;
    return c;
}

static void accept_loop(int ep, int lfd) {
    for (;;) {
        struct sockaddr_storage ss;
        socklen_t slen = sizeof(ss);
        int cfd = accept4(lfd, (struct sockaddr *)&ss, &slen, SOCK_NONBLOCK | SOCK_CLOEXEC);
        if (cfd < 0) {
            if (errno == EAGAIN || errno == EWOULDBLOCK) return;
            continue;
        }
        enable_nodelay(cfd, ss.ss_family);

        unsigned idx = rr_cursor++ & 1u;
        int bfd = make_backend_socket(idx);
        if (bfd < 0 || cfd >= MAX_FD || bfd >= MAX_FD) {
            close(cfd);
            if (bfd >= 0) close(bfd);
            continue;
        }

        Conn *client = new_conn(cfd, bfd, 0, 1);
        Conn *backend = new_conn(bfd, cfd, 1, 0);
        if (!client || !backend || add_epoll(ep, client, 0) < 0 || add_epoll(ep, backend, EPOLLOUT) < 0) {
            close_pair(ep, cfd);
            continue;
        }
    }
}

static void handle_read(int ep, Conn *c) {
    unsigned char tmp[BUF_SIZE];
    for (;;) {
        ssize_t n = read(c->fd, tmp, sizeof(tmp));
        if (n > 0) {
            if (c->peer < 0 || c->peer >= MAX_FD || !fdtab[c->peer]) {
                close_pair(ep, c->fd);
                return;
            }
            Conn *p = fdtab[c->peer];
            if (p->off > 0 && p->off == p->len) {
                p->off = 0;
                p->len = 0;
            }
            if ((size_t)n > sizeof(p->buf) - p->len) {
                close_pair(ep, c->fd);
                return;
            }
            memcpy(p->buf + p->len, tmp, (size_t)n);
            p->len += (size_t)n;
            mod_epoll(ep, p);
            continue;
        }
        if (n == 0) {
            if (c->peer >= 0 && c->peer < MAX_FD && fdtab[c->peer] && fdtab[c->peer]->len > fdtab[c->peer]->off) {
                Conn *p = fdtab[c->peer];
                p->close_after_write = 1;
                mod_epoll(ep, p);
                free_conn(ep, c->fd);
            } else {
                close_pair(ep, c->fd);
            }
            return;
        }
        if (errno == EAGAIN || errno == EWOULDBLOCK) return;
        close_pair(ep, c->fd);
        return;
    }
}

static int finish_connect(Conn *c) {
    int err = 0;
    socklen_t len = sizeof(err);
    if (getsockopt(c->fd, SOL_SOCKET, SO_ERROR, &err, &len) < 0) return -1;
    if (err != 0) {
        errno = err;
        return -1;
    }
    c->connected = 1;
    return 0;
}

static void handle_write(int ep, Conn *c) {
    if (c->is_backend && !c->connected && finish_connect(c) < 0) {
        close_pair(ep, c->fd);
        return;
    }
    while (c->off < c->len) {
        ssize_t n = write(c->fd, c->buf + c->off, c->len - c->off);
        if (n > 0) {
            c->off += (size_t)n;
            continue;
        }
        if (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) break;
        close_pair(ep, c->fd);
        return;
    }
    if (c->off == c->len) {
        c->off = 0;
        c->len = 0;
    }
    if (c->close_after_write) {
        free_conn(ep, c->fd);
        return;
    }
    mod_epoll(ep, c);
}

static int listen_socket(const char *addr, const char *port) {
    struct addrinfo hints;
    struct addrinfo *res = NULL;
    memset(&hints, 0, sizeof(hints));
    hints.ai_family = AF_UNSPEC;
    hints.ai_socktype = SOCK_STREAM;
    hints.ai_flags = AI_PASSIVE;
    int rc = getaddrinfo(addr, port, &hints, &res);
    if (rc != 0 || !res) {
        fprintf(stderr, "listen getaddrinfo: %s\n", gai_strerror(rc));
        exit(2);
    }

    int fd = -1;
    for (struct addrinfo *it = res; it; it = it->ai_next) {
        fd = socket(it->ai_family, it->ai_socktype | SOCK_NONBLOCK | SOCK_CLOEXEC, it->ai_protocol);
        if (fd < 0) continue;
        int one = 1;
        setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &one, sizeof(one));
        if (bind(fd, it->ai_addr, it->ai_addrlen) == 0 && listen(fd, 4096) == 0) {
#ifdef TCP_DEFER_ACCEPT
            int defer = 1;
            setsockopt(fd, IPPROTO_TCP, TCP_DEFER_ACCEPT, &defer, sizeof(defer));
#endif
            break;
        }
        close(fd);
        fd = -1;
    }
    freeaddrinfo(res);
    if (fd < 0) {
        perror("listen");
        exit(2);
    }
    return fd;
}

int main(void) {
    const char *b1 = getenv("BACKEND1");
    const char *b2 = getenv("BACKEND2");
    const char *addr = getenv("LB_ADDR");
    const char *port = getenv("LB_PORT");
    if (!b1) b1 = "127.0.0.1:8081";
    if (!b2) b2 = "127.0.0.1:8082";
    if (!addr) addr = "0.0.0.0";
    if (!port) port = "9999";

    resolve_backend(0, b1);
    resolve_backend(1, b2);

    int lfd = listen_socket(addr, port);
    int ep = epoll_create1(EPOLL_CLOEXEC);
    if (ep < 0) {
        perror("epoll_create1");
        return 2;
    }

    struct epoll_event ev;
    memset(&ev, 0, sizeof(ev));
    ev.events = EPOLLIN;
    ev.data.fd = lfd;
    if (epoll_ctl(ep, EPOLL_CTL_ADD, lfd, &ev) < 0) {
        perror("epoll_ctl listen");
        return 2;
    }

    fprintf(stderr, "lb listening on %s:%s -> %s,%s\n", addr, port, b1, b2);
    struct epoll_event events[MAX_EVENTS];
    for (;;) {
        int n = epoll_wait(ep, events, MAX_EVENTS, -1);
        if (n < 0) {
            if (errno == EINTR) continue;
            perror("epoll_wait");
            return 2;
        }
        for (int i = 0; i < n; i++) {
            int fd = events[i].data.fd;
            uint32_t e = events[i].events;
            if (fd == lfd) {
                accept_loop(ep, lfd);
                continue;
            }
            if (fd < 0 || fd >= MAX_FD || !fdtab[fd]) continue;
            Conn *c = fdtab[fd];
            if (e & EPOLLERR) {
                close_pair(ep, fd);
                continue;
            }
            if (e & EPOLLOUT) {
                handle_write(ep, c);
                if (fd < 0 || fd >= MAX_FD || !fdtab[fd]) continue;
            }
            if (e & (EPOLLIN | EPOLLHUP | EPOLLRDHUP)) {
                handle_read(ep, fdtab[fd]);
            }
        }
    }
}
