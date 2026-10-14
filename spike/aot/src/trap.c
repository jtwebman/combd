// combd guard-page traps. A cell's Wasm code touches memory with no bounds
// checks; an access past the live memory hits a PROT_NONE guard page. The
// signal handler below turns that fault into a trap, but only when the address
// is inside the reservation of the cell running on this thread. Any other
// fault goes to the previous handler, so real host crashes still crash.
#include <setjmp.h>
#include <signal.h>
#include <stddef.h>
#include <string.h>

typedef void (*combd_cb)(void *);

static __thread sigjmp_buf *combd_jmp;
static __thread unsigned char *combd_lo, *combd_hi;
static struct sigaction combd_prev_segv, combd_prev_bus;

static void combd_handler(int sig, siginfo_t *info, void *uctx) {
    unsigned char *a = (unsigned char *)info->si_addr;
    if (combd_jmp && a >= combd_lo && a < combd_hi) {
        siglongjmp(*combd_jmp, 1);
    }
    struct sigaction *prev = sig == SIGSEGV ? &combd_prev_segv : &combd_prev_bus;
    if ((prev->sa_flags & SA_SIGINFO) && prev->sa_sigaction) {
        prev->sa_sigaction(sig, info, uctx);
    } else if (prev->sa_handler != SIG_DFL && prev->sa_handler != SIG_IGN) {
        prev->sa_handler(sig);
    } else {
        // Restore the default and return: the access faults again and the
        // process dies, as it would have without us.
        sigaction(sig, prev, NULL);
    }
}

void combd_install(void) {
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = combd_handler;
    // SA_NODEFER: the fault signal is never blocked while the handler runs,
    // so jumping out leaves the mask as it was, and setjmp needn't save it.
    sa.sa_flags = SA_SIGINFO | SA_NODEFER;
    sigemptyset(&sa.sa_mask);
    sigaction(SIGSEGV, &sa, &combd_prev_segv);
    sigaction(SIGBUS, &sa, &combd_prev_bus);
}

// Run cb(arg) with [lo, lo + len) as this thread's cell reservation.
// Returns 0 if it finished, 1 if a guard-page fault turned into a trap.
int combd_catch(combd_cb cb, void *arg, void *lo, size_t len) {
    sigjmp_buf buf;
    sigjmp_buf *prev_jmp = combd_jmp;
    unsigned char *prev_lo = combd_lo, *prev_hi = combd_hi;
    // 0: don't save the signal mask. Saving it costs a system call per call.
    if (sigsetjmp(buf, 0)) {
        combd_jmp = prev_jmp;
        combd_lo = prev_lo;
        combd_hi = prev_hi;
        return 1;
    }
    combd_lo = (unsigned char *)lo;
    combd_hi = (unsigned char *)lo + len;
    combd_jmp = &buf;
    cb(arg);
    combd_jmp = prev_jmp;
    combd_lo = prev_lo;
    combd_hi = prev_hi;
    return 0;
}
