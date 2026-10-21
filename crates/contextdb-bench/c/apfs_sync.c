// ADR 0008 §17 evidence: cost of fsync, F_BARRIERFSYNC and F_FULLFSYNC on
// macOS/APFS, and of one file per payload versus one pack append per group,
// alone and with a concurrent F_FULLFSYNC loop (the session log). macOS only.
//   cc -O2 crates/contextdb-bench/c/apfs_sync.c -o /tmp/apfs_sync && /tmp/apfs_sync <dir>
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static double now(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec * 1e3 + t.tv_nsec / 1e6;
}
static char dir[512];
static volatile int stop;
static void path(char *out, const char *name) { snprintf(out, 600, "%s/%s", dir, name); }
static void *contend(void *x) {
    (void)x;
    char p[600];
    path(p, "bench-session");
    int s = open(p, O_CREAT | O_WRONLY | O_APPEND, 0644);
    char b[200] = {2};
    while (!stop) {
        write(s, b, sizeof b);
        fcntl(s, F_FULLFSYNC);
    }
    close(s);
    return 0;
}
static void groups(int contended) {
    char p[600], o[600];
    path(p, "bench-pack");
    path(o, "bench-oplog");
    int pack = open(p, O_CREAT | O_WRONLY | O_APPEND, 0644);
    int log = open(o, O_CREAT | O_WRONLY | O_APPEND, 0644);
    int d = open(dir, O_RDONLY);
    char buf[300] = {1};
    const int N = 100, K = 7;
    double files = 0, packs = 0;
    for (int i = 0; i < N; i++) {
        double t0 = now();
        for (int k = 0; k < K; k++) {
            char n[640], m[640];
            snprintf(n, sizeof n, "%s/bench-blob-%d-%d.tmp", dir, i, k);
            snprintf(m, sizeof m, "%s/bench-blob-%d-%d", dir, i, k);
            int f = open(n, O_CREAT | O_WRONLY, 0644);
            write(f, buf, sizeof buf);
            fcntl(f, F_BARRIERFSYNC);
            close(f);
            rename(n, m);
            unlink(m);
        }
        fcntl(d, F_BARRIERFSYNC);
        files += now() - t0;
        write(log, buf, sizeof buf);
        fcntl(log, F_FULLFSYNC);
        t0 = now();
        for (int k = 0; k < K; k++) write(pack, buf, sizeof buf);
        fcntl(pack, F_BARRIERFSYNC);
        packs += now() - t0;
        write(log, buf, sizeof buf);
        fcntl(log, F_FULLFSYNC);
    }
    printf("%s: %d payloads per group: file per payload %.2f ms, pack append %.2f ms\n",
           contended ? "with a concurrent F_FULLFSYNC loop" : "alone", K, files / N, packs / N);
    close(pack), close(log), close(d);
    unlink(p), unlink(o);
}
int main(int argc, char **argv) {
    snprintf(dir, sizeof dir, "%s", argc > 1 ? argv[1] : ".");
    char p[600];
    path(p, "bench-sync");
    int fd = open(p, O_CREAT | O_WRONLY | O_APPEND, 0644);
    char b[200] = {1};
    const char *names[] = {"fsync", "F_FULLFSYNC", "F_BARRIERFSYNC"};
    for (int mode = 0; mode < 3; mode++) {
        double t0 = now();
        for (int i = 0; i < 200; i++) {
            write(fd, b, sizeof b);
            if (mode == 0) fsync(fd);
            else fcntl(fd, mode == 1 ? F_FULLFSYNC : F_BARRIERFSYNC);
        }
        printf("append + %s: %.3f ms\n", names[mode], (now() - t0) / 200);
    }
    close(fd);
    unlink(p);
    groups(0);
    pthread_t t;
    pthread_create(&t, 0, contend, 0);
    groups(1);
    stop = 1;
    pthread_join(t, 0);
    path(p, "bench-session");
    unlink(p);
    return 0;
}
