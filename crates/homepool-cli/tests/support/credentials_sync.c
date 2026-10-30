// Test-only Linux fsync fault injection; never linked into the shipped binary.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

int fsync(int fd) {
    const char *fault = getenv("HOMEPOOL_TEST_SYNC_FAULT");
    struct stat actual, destination;
    if (fault == NULL || fstat(fd, &actual) != 0 ||
        stat("credentials.json", &destination) != 0 ||
        actual.st_dev != destination.st_dev || actual.st_ino != destination.st_ino ||
        !S_ISREG(actual.st_mode) || actual.st_size == 0) {
        return (int)syscall(SYS_fsync, fd);
    }

    // Prove the test reached credentials sync after a successful nonempty write.
    int marker = open("credentials-sync-reached", O_WRONLY | O_CREAT | O_EXCL, 0600);
    if (marker < 0 || close(marker) != 0) {
        _exit(99);
    }
    if (strcmp(fault, "kill") == 0) {
        kill(getpid(), SIGKILL);
        _exit(99);
    }
    if (strcmp(fault, "term-error") == 0) {
        // Self-directed, unblocked SIGTERM runs the CLI's handlers before kill
        // returns. Only then return EIO, so cancellation and sync failure coexist.
        if (kill(getpid(), SIGTERM) != 0) {
            _exit(99);
        }
        errno = EIO;
        return -1;
    }
    _exit(99);
}
