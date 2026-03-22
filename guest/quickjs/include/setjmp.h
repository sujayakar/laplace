#pragma once
// aarch64: need 22 64-bit registers (x19-x30, d8-d15, sp, lr, etc.)
typedef long long jmp_buf[32];
int setjmp(jmp_buf env);
void longjmp(jmp_buf env, int val) __attribute__((noreturn));
