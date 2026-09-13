#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/sendfile.h>
#include <sys/types.h>
#include <unistd.h>

/* Loaded only into an isolated integration-test child. No device or production hook. */
static atomic_uint sequence;
static int watched(const char *path) {
    const char *prefix = getenv("MICA_FAULT_PREFIX");
    if (!prefix || !path) return 0;
    size_t n = strlen(prefix);
    return !strncmp(path, prefix, n) && (path[n] == '/' || path[n] == '\0');
}
static int watched_fd(int fd) {
    char name[64], path[PATH_MAX];
    snprintf(name, sizeof name, "/proc/self/fd/%d", fd);
    ssize_t length = readlink(name, path, sizeof path - 1);
    if (length < 0) return 0;
    path[length] = '\0';
    return watched(path);
}
static unsigned begin(const char *kind, int active) {
    if (!active) return 0;
    unsigned n = atomic_fetch_add(&sequence, 1) + 1;
    const char *log = getenv("MICA_FAULT_LOG");
    int fd = open(log, O_CREAT | O_APPEND | O_WRONLY, 0600);
    if (fd < 0) _exit(98);
    char line[96];
    int length = snprintf(line, sizeof line, "%u %s\n", n, kind);
    ssize_t (*real_write)(int, const void *, size_t) = dlsym(RTLD_NEXT, "write");
    if (real_write(fd, line, (size_t)length) != length) _exit(98);
    close(fd);
    return n;
}
static int fault(unsigned n, const char *when) {
    const char *at = getenv("MICA_FAULT_AT"), *phase = getenv("MICA_FAULT_WHEN");
    if (!n || !at || !phase || strtoul(at, NULL, 10) != n || strcmp(phase, when)) return 0;
    if (getenv("MICA_FAULT_ENOSPC")) { errno = ENOSPC; return 1; }
    kill(getpid(), SIGKILL);
    _exit(97);
}
#define BEFORE(kind, active) unsigned n = begin(kind, active); if (fault(n, "before")) return -1
#define AFTER(result) do { int saved = errno; if (fault(n, "after")) return -1; errno = saved; return result; } while (0)
ssize_t write(int fd, const void *buf, size_t length) {
    ssize_t (*real)(int,const void *,size_t) = dlsym(RTLD_NEXT,"write");
    BEFORE("write", watched_fd(fd)); ssize_t result = real(fd,buf,length); AFTER(result);
}
ssize_t pwrite64(int fd, const void *buf, size_t length, off64_t offset) {
    ssize_t (*real)(int,const void *,size_t,off64_t) = dlsym(RTLD_NEXT,"pwrite64");
    BEFORE("pwrite64", watched_fd(fd)); ssize_t result = real(fd,buf,length,offset); AFTER(result);
}
ssize_t copy_file_range(int src, off64_t *src_off, int dst, off64_t *dst_off, size_t length, unsigned flags) {
    ssize_t (*real)(int,off64_t *,int,off64_t *,size_t,unsigned) = dlsym(RTLD_NEXT,"copy_file_range");
    BEFORE("copy_file_range", watched_fd(dst)); ssize_t result = real(src,src_off,dst,dst_off,length,flags); AFTER(result);
}
ssize_t sendfile64(int dst, int src, off64_t *offset, size_t length) {
    ssize_t (*real)(int,int,off64_t *,size_t) = dlsym(RTLD_NEXT,"sendfile64");
    BEFORE("sendfile64", watched_fd(dst)); ssize_t result = real(dst,src,offset,length); AFTER(result);
}
int fsync(int fd) {
    int (*real)(int) = dlsym(RTLD_NEXT,"fsync");
    BEFORE("fsync", watched_fd(fd)); int result = real(fd); AFTER(result);
}
int fdatasync(int fd) {
    int (*real)(int) = dlsym(RTLD_NEXT,"fdatasync");
    BEFORE("fdatasync", watched_fd(fd)); int result = real(fd); AFTER(result);
}
int rename(const char *old, const char *next) {
    int (*real)(const char *,const char *) = dlsym(RTLD_NEXT,"rename");
    BEFORE("rename", watched(old) || watched(next)); int result = real(old,next); AFTER(result);
}
int mkdir(const char *path, mode_t mode) {
    int (*real)(const char *,mode_t) = dlsym(RTLD_NEXT,"mkdir");
    BEFORE("mkdir", watched(path)); int result = real(path,mode); AFTER(result);
}
int unlink(const char *path) {
    int (*real)(const char *) = dlsym(RTLD_NEXT,"unlink");
    BEFORE("unlink", watched(path)); int result = real(path); AFTER(result);
}
int rmdir(const char *path) {
    int (*real)(const char *) = dlsym(RTLD_NEXT,"rmdir");
    BEFORE("rmdir", watched(path)); int result = real(path); AFTER(result);
}
