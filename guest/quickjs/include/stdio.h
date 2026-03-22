#pragma once
#include <stddef.h>
#include <stdarg.h>

typedef struct _FILE FILE;
extern FILE *stdout;
extern FILE *stderr;
#define EOF (-1)

int printf(const char *fmt, ...) __attribute__((format(printf, 1, 2)));
int fprintf(FILE *stream, const char *fmt, ...) __attribute__((format(printf, 2, 3)));
int snprintf(char *str, size_t size, const char *fmt, ...) __attribute__((format(printf, 3, 4)));
int vsnprintf(char *str, size_t size, const char *fmt, va_list ap);
int vfprintf(FILE *stream, const char *fmt, va_list ap);
int puts(const char *s);
int fputs(const char *s, FILE *stream);
int fputc(int c, FILE *stream);
size_t fwrite(const void *ptr, size_t size, size_t nmemb, FILE *stream);
int putchar(int c);
int getc(FILE *stream);

