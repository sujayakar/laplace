#pragma once
#include <stddef.h>
#include <stdint.h>

typedef int64_t time_t;

struct tm {
    int tm_sec;
    int tm_min;
    int tm_hour;
    int tm_mday;
    int tm_mon;
    int tm_year;
    int tm_wday;
    int tm_yday;
    int tm_isdst;
    long tm_gmtoff;
    const char *tm_zone;
};

typedef int clockid_t;
struct timespec {
    time_t tv_sec;
    long tv_nsec;
};

#define CLOCK_MONOTONIC 1

time_t time(time_t *tloc);
time_t mktime(struct tm *tm);
struct tm *gmtime_r(const time_t *timep, struct tm *result);
struct tm *localtime_r(const time_t *timep, struct tm *result);
size_t strftime(char *s, size_t max, const char *format, const struct tm *tm);
int clock_gettime(clockid_t clk_id, struct timespec *tp);
