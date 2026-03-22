#pragma once
void _guest_abort(void) __attribute__((noreturn));
#define assert(expr) ((expr) ? (void)0 : _guest_abort())
