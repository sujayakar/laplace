#pragma once
#include <stddef.h>

void *malloc(size_t size);
void *calloc(size_t count, size_t size);
void *realloc(void *ptr, size_t size);
void free(void *ptr);
void abort(void) __attribute__((noreturn));
void exit(int status) __attribute__((noreturn));
long strtol(const char *str, char **endptr, int base);
unsigned long strtoul(const char *str, char **endptr, int base);
long long strtoll(const char *str, char **endptr, int base);
unsigned long long strtoull(const char *str, char **endptr, int base);
double strtod(const char *str, char **endptr);
int abs(int j);
long labs(long j);
void qsort(void *base, size_t nmemb, size_t size, int (*compar)(const void *, const void *));

#define RAND_MAX 2147483647
int rand(void);
void srand(unsigned int seed);

#define EXIT_SUCCESS 0
#define EXIT_FAILURE 1
