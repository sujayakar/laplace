/*
 * Minimal libc stubs for QuickJS running in a bare-metal aarch64 guest.
 * Provides the C library functions that QuickJS depends on.
 * Memory functions (malloc/free/realloc) are implemented in Rust and linked.
 * Math functions are provided by the `libm` Rust crate.
 * This file provides: string ops, formatting, time stubs, ctype, qsort, etc.
 */

#include <stddef.h>
#include <stdint.h>
#include <stdarg.h>

/* ── errno ────────────────────────────────────────────────────── */
int errno = 0;

/* ── stdio stubs (FILE, stdout, stderr) ──────────────────────── */
typedef struct _FILE FILE;
static char _stdout_dummy;
static char _stderr_dummy;
FILE *stdout = (FILE *)&_stdout_dummy;
FILE *stderr = (FILE *)&_stderr_dummy;

/* These are implemented in Rust via extern "C" */
extern void _guest_console_write(const char *s, size_t len);
extern void _guest_abort(void);

int putchar(int c) {
    char ch = (char)c;
    _guest_console_write(&ch, 1);
    return c;
}

int puts(const char *s) {
    size_t len = 0;
    while (s[len]) len++;
    _guest_console_write(s, len);
    _guest_console_write("\n", 1);
    return 0;
}

int fputs(const char *s, FILE *stream) {
    size_t len = 0;
    while (s[len]) len++;
    _guest_console_write(s, len);
    return 0;
}

int fputc(int c, FILE *stream) {
    return putchar(c);
}

size_t fwrite(const void *ptr, size_t size, size_t nmemb, FILE *stream) {
    _guest_console_write((const char *)ptr, size * nmemb);
    return nmemb;
}

int getc(FILE *stream) {
    return -1; /* EOF */
}

/* ── printf family via vsnprintf ─────────────────────────────── */

/* Minimal vsnprintf — handles %s, %d, %u, %x, %f, %c, %p, %ld, %lld, etc.
   This is a simplistic implementation sufficient for QuickJS's internal use. */

static int fmt_int(char *buf, size_t size, size_t pos, long long val, int is_unsigned, int base, int width, int zero_pad, int is_upper) {
    char tmp[24];
    int neg = 0;
    int len = 0;
    unsigned long long uval;

    if (!is_unsigned && val < 0) {
        neg = 1;
        uval = (unsigned long long)(-val);
    } else {
        uval = (unsigned long long)val;
    }

    if (uval == 0) {
        tmp[len++] = '0';
    } else {
        const char *digits = is_upper ? "0123456789ABCDEF" : "0123456789abcdef";
        while (uval > 0) {
            tmp[len++] = digits[uval % base];
            uval /= base;
        }
    }

    int total = len + neg;
    int pad = (width > total) ? width - total : 0;

    int written = 0;
    if (!zero_pad) {
        for (int i = 0; i < pad && pos < size - 1; i++, pos++) { buf[pos] = ' '; written++; }
    }
    if (neg && pos < size - 1) { buf[pos++] = '-'; written++; }
    if (zero_pad) {
        for (int i = 0; i < pad && pos < size - 1; i++, pos++) { buf[pos] = '0'; written++; }
    }
    for (int i = len - 1; i >= 0 && pos < size - 1; i--, pos++) { buf[pos] = tmp[i]; written++; }

    return written;
}

int vsnprintf(char *buf, size_t size, const char *fmt, va_list ap) {
    if (size == 0) return 0;
    size_t pos = 0;

    while (*fmt && pos < size - 1) {
        if (*fmt != '%') {
            buf[pos++] = *fmt++;
            continue;
        }
        fmt++; /* skip '%' */

        /* flags */
        int zero_pad = 0;
        int left_align = 0;
        int plus_sign = 0;
        while (*fmt == '0' || *fmt == '-' || *fmt == '+' || *fmt == ' ') {
            if (*fmt == '0') zero_pad = 1;
            if (*fmt == '-') left_align = 1;
            if (*fmt == '+') plus_sign = 1;
            fmt++;
        }

        /* width */
        int width = 0;
        if (*fmt == '*') {
            width = va_arg(ap, int);
            fmt++;
        } else {
            while (*fmt >= '0' && *fmt <= '9') {
                width = width * 10 + (*fmt - '0');
                fmt++;
            }
        }

        /* precision */
        int precision = -1;
        if (*fmt == '.') {
            fmt++;
            precision = 0;
            if (*fmt == '*') {
                precision = va_arg(ap, int);
                fmt++;
            } else {
                while (*fmt >= '0' && *fmt <= '9') {
                    precision = precision * 10 + (*fmt - '0');
                    fmt++;
                }
            }
        }

        /* length modifier */
        int is_long = 0;
        int is_longlong = 0;
        int is_size_t = 0;
        if (*fmt == 'l') { fmt++; is_long = 1; if (*fmt == 'l') { fmt++; is_longlong = 1; } }
        else if (*fmt == 'z') { fmt++; is_size_t = 1; }
        else if (*fmt == 'j') { fmt++; is_longlong = 1; }

        /* conversion */
        switch (*fmt) {
        case 'd': case 'i': {
            long long val;
            if (is_longlong) val = va_arg(ap, long long);
            else if (is_long || is_size_t) val = va_arg(ap, long);
            else val = va_arg(ap, int);
            pos += fmt_int(buf, size, pos, val, 0, 10, width, zero_pad, 0);
            break;
        }
        case 'u': {
            unsigned long long val;
            if (is_longlong) val = va_arg(ap, unsigned long long);
            else if (is_long || is_size_t) val = va_arg(ap, unsigned long);
            else val = va_arg(ap, unsigned int);
            pos += fmt_int(buf, size, pos, (long long)val, 1, 10, width, zero_pad, 0);
            break;
        }
        case 'x': case 'X': {
            unsigned long long val;
            if (is_longlong) val = va_arg(ap, unsigned long long);
            else if (is_long || is_size_t) val = va_arg(ap, unsigned long);
            else val = va_arg(ap, unsigned int);
            pos += fmt_int(buf, size, pos, (long long)val, 1, 16, width, zero_pad, *fmt == 'X');
            break;
        }
        case 's': {
            const char *s = va_arg(ap, const char *);
            if (!s) s = "(null)";
            int slen = 0;
            while (s[slen]) slen++;
            if (precision >= 0 && slen > precision) slen = precision;
            int pad = (width > slen) ? width - slen : 0;
            if (!left_align) for (int i = 0; i < pad && pos < size - 1; i++) buf[pos++] = ' ';
            for (int i = 0; i < slen && pos < size - 1; i++) buf[pos++] = s[i];
            if (left_align) for (int i = 0; i < pad && pos < size - 1; i++) buf[pos++] = ' ';
            break;
        }
        case 'c': {
            int c = va_arg(ap, int);
            buf[pos++] = (char)c;
            break;
        }
        case 'p': {
            void *ptr = va_arg(ap, void *);
            if (pos < size - 1) buf[pos++] = '0';
            if (pos < size - 1) buf[pos++] = 'x';
            pos += fmt_int(buf, size, pos, (long long)(uintptr_t)ptr, 1, 16, 0, 0, 0);
            break;
        }
        case 'f': case 'g': case 'e': {
            /* Minimal float formatting — QuickJS uses its own dtoa for most paths */
            double val = va_arg(ap, double);
            /* Just format as integer part for now — dtoa.c handles the real formatting */
            long long ival = (long long)val;
            pos += fmt_int(buf, size, pos, ival, 0, 10, 0, 0, 0);
            break;
        }
        case '%':
            buf[pos++] = '%';
            break;
        case 'n':
            /* ignore */
            break;
        default:
            /* Unknown format, skip */
            break;
        }
        fmt++;
    }
    buf[pos] = '\0';
    return (int)pos;
}

int snprintf(char *buf, size_t size, const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    int ret = vsnprintf(buf, size, fmt, ap);
    va_end(ap);
    return ret;
}

int printf(const char *fmt, ...) {
    char buf[1024];
    va_list ap;
    va_start(ap, fmt);
    int ret = vsnprintf(buf, sizeof(buf), fmt, ap);
    va_end(ap);
    _guest_console_write(buf, ret);
    return ret;
}

int fprintf(FILE *stream, const char *fmt, ...) {
    char buf[1024];
    va_list ap;
    va_start(ap, fmt);
    int ret = vsnprintf(buf, sizeof(buf), fmt, ap);
    va_end(ap);
    _guest_console_write(buf, ret);
    return ret;
}

int vfprintf(FILE *stream, const char *fmt, va_list ap) {
    char buf[1024];
    int ret = vsnprintf(buf, sizeof(buf), fmt, ap);
    _guest_console_write(buf, ret);
    return ret;
}

/* ── string functions ────────────────────────────────────────── */

void *memchr(const void *s, int c, size_t n) {
    const unsigned char *p = (const unsigned char *)s;
    for (size_t i = 0; i < n; i++) {
        if (p[i] == (unsigned char)c) return (void *)(p + i);
    }
    return NULL;
}

char *strchr(const char *s, int c) {
    while (*s) {
        if (*s == (char)c) return (char *)s;
        s++;
    }
    return (c == 0) ? (char *)s : NULL;
}

char *strrchr(const char *s, int c) {
    const char *last = NULL;
    while (*s) {
        if (*s == (char)c) last = s;
        s++;
    }
    return (char *)last;
}

char *strstr(const char *haystack, const char *needle) {
    if (!*needle) return (char *)haystack;
    size_t nlen = 0;
    while (needle[nlen]) nlen++;
    while (*haystack) {
        int match = 1;
        for (size_t i = 0; i < nlen; i++) {
            if (haystack[i] != needle[i]) { match = 0; break; }
        }
        if (match) return (char *)haystack;
        haystack++;
    }
    return NULL;
}

char *strncpy(char *dest, const char *src, size_t n) {
    size_t i;
    for (i = 0; i < n && src[i]; i++) dest[i] = src[i];
    for (; i < n; i++) dest[i] = '\0';
    return dest;
}

char *strcpy(char *dest, const char *src) {
    char *ret = dest;
    while ((*dest++ = *src++));
    return ret;
}

char *strcat(char *dest, const char *src) {
    char *ret = dest;
    while (*dest) dest++;
    while ((*dest++ = *src++));
    return ret;
}

int strcmp(const char *s1, const char *s2) {
    while (*s1 && *s1 == *s2) { s1++; s2++; }
    return (unsigned char)*s1 - (unsigned char)*s2;
}

int strncmp(const char *s1, const char *s2, size_t n) {
    for (size_t i = 0; i < n; i++) {
        if (s1[i] != s2[i]) return (unsigned char)s1[i] - (unsigned char)s2[i];
        if (s1[i] == '\0') return 0;
    }
    return 0;
}

size_t strlen(const char *s) {
    size_t len = 0;
    while (s[len]) len++;
    return len;
}

/* strdup needs malloc — implemented in Rust, declared extern */
extern void *malloc(size_t size);

char *strdup(const char *s) {
    size_t len = 0;
    while (s[len]) len++;
    char *d = (char *)malloc(len + 1);
    if (d) {
        for (size_t i = 0; i <= len; i++) d[i] = s[i];
    }
    return d;
}

/* ── strtol / strtod family ──────────────────────────────────── */

long strtol(const char *str, char **endptr, int base) {
    long result = 0;
    int neg = 0;
    while (*str == ' ' || *str == '\t') str++;
    if (*str == '-') { neg = 1; str++; }
    else if (*str == '+') str++;

    if (base == 0) {
        if (*str == '0' && (str[1] == 'x' || str[1] == 'X')) { base = 16; str += 2; }
        else if (*str == '0') { base = 8; }
        else base = 10;
    } else if (base == 16 && *str == '0' && (str[1] == 'x' || str[1] == 'X')) {
        str += 2;
    }

    while (*str) {
        int digit;
        if (*str >= '0' && *str <= '9') digit = *str - '0';
        else if (*str >= 'a' && *str <= 'f') digit = *str - 'a' + 10;
        else if (*str >= 'A' && *str <= 'F') digit = *str - 'A' + 10;
        else break;
        if (digit >= base) break;
        result = result * base + digit;
        str++;
    }
    if (endptr) *endptr = (char *)str;
    return neg ? -result : result;
}

unsigned long strtoul(const char *str, char **endptr, int base) {
    return (unsigned long)strtol(str, endptr, base);
}

long long strtoll(const char *str, char **endptr, int base) {
    return (long long)strtol(str, endptr, base);
}

unsigned long long strtoull(const char *str, char **endptr, int base) {
    return (unsigned long long)strtol(str, endptr, base);
}

/* strtod is complex — QuickJS has its own dtoa.c for this, so a stub is fine
   for any non-dtoa usage */
double strtod(const char *str, char **endptr) {
    /* Minimal: parse integer part only */
    double result = 0;
    int neg = 0;
    while (*str == ' ') str++;
    if (*str == '-') { neg = 1; str++; }
    else if (*str == '+') str++;
    while (*str >= '0' && *str <= '9') {
        result = result * 10 + (*str - '0');
        str++;
    }
    if (*str == '.') {
        str++;
        double frac = 0.1;
        while (*str >= '0' && *str <= '9') {
            result += (*str - '0') * frac;
            frac *= 0.1;
            str++;
        }
    }
    if (endptr) *endptr = (char *)str;
    return neg ? -result : result;
}

/* ── ctype ───────────────────────────────────────────────────── */

int isdigit(int c) { return c >= '0' && c <= '9'; }
int isalpha(int c) { return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z'); }
int isalnum(int c) { return isdigit(c) || isalpha(c); }
int isspace(int c) { return c == ' ' || c == '\t' || c == '\n' || c == '\r' || c == '\f' || c == '\v'; }
int isupper(int c) { return c >= 'A' && c <= 'Z'; }
int islower(int c) { return c >= 'a' && c <= 'z'; }
int isxdigit(int c) { return isdigit(c) || (c >= 'a' && c <= 'f') || (c >= 'A' && c <= 'F'); }
int tolower(int c) { return isupper(c) ? c + 32 : c; }
int toupper(int c) { return islower(c) ? c - 32 : c; }

/* ── qsort ───────────────────────────────────────────────────── */
/* Simple insertion sort — fine for QuickJS's internal use */

void qsort(void *base, size_t nmemb, size_t size, int (*compar)(const void *, const void *)) {
    char *arr = (char *)base;
    /* Heap-allocate swap buffer to handle any element size */
    char *tmp = (char *)malloc(size);
    if (!tmp) return;
    for (size_t i = 1; i < nmemb; i++) {
        for (size_t j = i; j > 0; j--) {
            char *a = arr + (j - 1) * size;
            char *b = arr + j * size;
            if (compar(a, b) <= 0) break;
            /* swap via tmp buffer */
            for (size_t k = 0; k < size; k++) { tmp[k] = a[k]; a[k] = b[k]; b[k] = tmp[k]; }
        }
    }
    /* free is a no-op (bump allocator), but call for correctness */
    free(tmp);
}

/* ── time stubs (deterministic) ──────────────────────────────── */

extern uint64_t _guest_get_time_ns(void);

/* Return deterministic time via hypercall */
long long time(long long *tloc) {
    uint64_t ns = _guest_get_time_ns();
    long long t = (long long)(ns / 1000000000ULL);
    if (tloc) *tloc = t;
    return t;
}

int gettimeofday(void *tv_void, void *tz) {
    uint64_t ns = _guest_get_time_ns();
    if (tv_void) {
        long long *tv = (long long *)tv_void;
        tv[0] = (long long)(ns / 1000000000ULL); /* tv_sec */
        tv[1] = (long)(((ns % 1000000000ULL) / 1000ULL)); /* tv_usec */
    }
    return 0;
}

int clock_gettime(int clk_id, void *tp_void) {
    uint64_t ns = _guest_get_time_ns();
    if (tp_void) {
        long long *tp = (long long *)tp_void;
        tp[0] = (long long)(ns / 1000000000ULL); /* tv_sec */
        tp[1] = (long)(ns % 1000000000ULL);       /* tv_nsec */
    }
    return 0;
}

long long mktime(void *tm) { return 0; }
void *localtime_r(const void *t, void *result) { return result; }

size_t strftime(char *s, size_t max, const char *fmt, const void *tm) {
    s[0] = '\0';
    return 0;
}

/* ── pthread stubs (single-threaded) ─────────────────────────── */

/* Debug: trace pthread_once calls */
int pthread_once(int *once, void (*fn)(void));

int pthread_mutex_init(void *m, const void *a) { return 0; }
int pthread_mutex_destroy(void *m) { return 0; }
int pthread_mutex_lock(void *m) { return 0; }
int pthread_mutex_unlock(void *m) { return 0; }
int pthread_once(int *once, void (*fn)(void)) { if (*once == 0) { *once = 1; fn(); } return 0; }
int pthread_cond_init(void *c, const void *a) { return 0; }
int pthread_cond_destroy(void *c) { return 0; }
int pthread_cond_signal(void *c) { return 0; }
int pthread_cond_broadcast(void *c) { return 0; }
int pthread_cond_wait(void *c, void *m) { return 0; }
int pthread_cond_timedwait(void *c, void *m, const void *t) { return 110; /* ETIMEDOUT */ }
int pthread_create(void *t, const void *a, void *(*fn)(void*), void *arg) { return -1; }
int pthread_join(unsigned long t, void **r) { return 0; }
int pthread_condattr_init(void *a) { return 0; }
int pthread_condattr_destroy(void *a) { return 0; }
int pthread_condattr_setclock(void *a, int c) { return 0; }
int pthread_attr_init(void *a) { return 0; }
int pthread_attr_destroy(void *a) { return 0; }
int pthread_attr_setdetachstate(void *a, int s) { return 0; }
int pthread_attr_setstacksize(void *a, unsigned long s) { return 0; }

/* ── misc ────────────────────────────────────────────────────── */

void abort(void) { _guest_abort(); }
void exit(int status) { _guest_abort(); }

int abs(int j) { return j < 0 ? -j : j; }
long labs(long j) { return j < 0 ? -j : j; }

static unsigned int _rand_state = 1;
int rand(void) {
    _rand_state = _rand_state * 1103515245 + 12345;
    return (_rand_state >> 16) & 0x7fff;
}
void srand(unsigned int seed) { _rand_state = seed; }

/* setjmp/longjmp — QuickJS needs these for exception handling.
   For aarch64, save/restore callee-saved registers.
   jmp_buf is declared in setjmp.h as long long[32]. */
__asm__(
    ".global setjmp\n"
    "setjmp:\n"
    "  stp x19, x20, [x0, #0]\n"
    "  stp x21, x22, [x0, #16]\n"
    "  stp x23, x24, [x0, #32]\n"
    "  stp x25, x26, [x0, #48]\n"
    "  stp x27, x28, [x0, #64]\n"
    "  stp x29, x30, [x0, #80]\n"
    "  mov x2, sp\n"
    "  str x2, [x0, #96]\n"
    "  stp d8, d9, [x0, #104]\n"
    "  stp d10, d11, [x0, #120]\n"
    "  stp d12, d13, [x0, #136]\n"
    "  stp d14, d15, [x0, #152]\n"
    "  mov x0, #0\n"
    "  ret\n"
);

__asm__(
    ".global longjmp\n"
    "longjmp:\n"
    "  ldp x19, x20, [x0, #0]\n"
    "  ldp x21, x22, [x0, #16]\n"
    "  ldp x23, x24, [x0, #32]\n"
    "  ldp x25, x26, [x0, #48]\n"
    "  ldp x27, x28, [x0, #64]\n"
    "  ldp x29, x30, [x0, #80]\n"
    "  ldr x2, [x0, #96]\n"
    "  mov sp, x2\n"
    "  ldp d8, d9, [x0, #104]\n"
    "  ldp d10, d11, [x0, #120]\n"
    "  ldp d12, d13, [x0, #136]\n"
    "  ldp d14, d15, [x0, #152]\n"
    "  mov x0, x1\n"
    "  cmp x0, #0\n"
    "  cinc x0, x0, eq\n"
    "  ret\n"
);
