#pragma once

#define INFINITY (__builtin_inff())
#define NAN (__builtin_nanf(""))
#define HUGE_VAL (__builtin_huge_val())
#define isnan(x) __builtin_isnan(x)
#define isinf(x) __builtin_isinf(x)
#define isfinite(x) __builtin_isfinite(x)
#define signbit(x) __builtin_signbit(x)

double sin(double x);
double cos(double x);
double tan(double x);
double asin(double x);
double acos(double x);
double atan(double x);
double atan2(double y, double x);
double pow(double x, double y);
double sqrt(double x);
double log(double x);
double log2(double x);
double log10(double x);
double log1p(double x);
double exp(double x);
double exp2(double x);
double expm1(double x);
double floor(double x);
double ceil(double x);
double round(double x);
double trunc(double x);
double fabs(double x);
double fmod(double x, double y);
double remainder(double x, double y);
double cbrt(double x);
double hypot(double x, double y);
double copysign(double x, double y);
double scalbn(double x, int n);
double ldexp(double x, int exp);
double frexp(double x, int *exp);
double modf(double x, double *iptr);
float fminf(float x, float y);
float fmaxf(float x, float y);
double fmin(double x, double y);
double fmax(double x, double y);
double nearbyint(double x);
double rint(double x);
long lrint(double x);
double cosh(double x);
double sinh(double x);
double tanh(double x);
double acosh(double x);
double asinh(double x);
double atanh(double x);
