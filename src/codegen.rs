//! Assembling a complete C program from the session's accumulated input.
//!
//! The model is accumulate-and-replay: every evaluation rebuilds and reruns
//! the whole program from scratch. Session variables are ordinary locals in
//! `main`; compiler-approved redeclarations open nested shadowing scopes that
//! enclose later statements. There is no symbol table and declarations are
//! never split from their initialisers.
//!
//! The cost is that side effects replay too. Markers written into the output
//! streams let the caller show only what the newest input produced.

use std::collections::BTreeSet;

use crate::lex;
use crate::session::Session;

/// Everything after this marker in a stream came from the newest input.
pub const M_NEW: &str = "\x01\x02c-shell/new\x02\x01";
/// Everything after this marker is the `Out[n]` value rather than program output.
pub const M_VAL: &str = "\x01\x02c-shell/val\x02\x01";
/// Reaching this marker proves that the newest input returned normally.
pub const M_DONE: &str = "\x01\x02c-shell/done\x02\x01";
/// A wrapped stdin call requests one recorded or live line from the parent.
pub const M_STDIN: &str = "\x01\x02c-shell/stdin\x02\x01";
/// Prefix for a bounded hexadecimal byte payload returned by the explicit
/// UTF-8 array probe. Rust validates and renders it before it reaches the UI.
pub const M_UTF8: &str = "\x01\x02c-shell/utf8\x02\x01";
/// Prefix for `%utf8`/`%utf16`/`%utf32` code-unit probe payloads.
pub const M_UNICODE: &str = "\x01\x02c-shell/unicode\x02\x01";

/// Headers every generated program includes before any user code, so beginners
/// need no `#include` for the common library. `%header` shows this list.
pub const HEADERS: &str = "\
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <math.h>
#include <stdbool.h>
#include <stdint.h>
#include <inttypes.h>
#include <stddef.h>
#include <limits.h>
#include <float.h>
#include <ctype.h>
#include <stdarg.h>
#include <time.h>
#if defined(__has_include)
#  if __has_include(<uchar.h>)
#include <uchar.h>
#  endif
#endif
#include <wchar.h>
";

/// Value printing dispatches through `_Generic` on the *function*, not on the
/// call. Selecting a call expression would make every unselected branch get
/// type-checked against the wrong argument type and fail to compile.
///
/// Each printer opens by marking the stream, and that placement is load
/// bearing: an argument is fully evaluated before the call it belongs to, so
/// anything the expression printed on its own way to a value has already been
/// flushed by the time the marker appears. Emitting the marker at the call
/// site instead would file `puts("hi")`'s own output under its return value.
const RUNTIME: &str = r##"
#define CS_VAL() fputs(CS_M_VAL, stdout)

static inline void cs_p_b (_Bool v)              { CS_VAL(); printf("%s\n", v ? "true" : "false"); }
static inline void cs_p_c (char v)               { CS_VAL(); printf("'%c' (%d)\n", (v >= 32 && v < 127) ? v : '?', (int)v); }
static inline void cs_p_i (long long v)          { CS_VAL(); printf("%lld\n", v); }
static inline void cs_p_u (unsigned long long v) { CS_VAL(); printf("%llu\n", v); }
static inline void cs_p_d (double v)             { CS_VAL(); printf("%.17g\n", v); }
static inline void cs_p_ld(long double v)        { CS_VAL(); printf("%.21Lg\n", v); }
static inline void cs_p_s (const char *v)        { CS_VAL(); if (v) printf("\"%s\"\n", v); else printf("NULL\n"); }
static inline void cs_p_p (const void *v)        { CS_VAL(); if (v) printf("0x%" PRIxPTR "\n", (uintptr_t)v); else printf("NULL\n"); }

#ifdef CS_ENABLE_BITS
static inline int cs_bits_little_endian(void)
{
    const uint16_t one = 1;
    return *(const unsigned char *)&one == 1;
}

static inline void cs_bits_dump(const void *object, size_t size, int uppercase)
{
    const unsigned char *bytes = (const unsigned char *)object;
    const int little = cs_bits_little_endian();
    size_t i;
    int bit;

    fputs(uppercase ? "hex: 0X" : "hex: 0x", stdout);
    for (i = 0; i < size; ++i) {
        const size_t index = little ? size - i - 1 : i;
        printf(uppercase ? "%02X" : "%02x", (unsigned)bytes[index]);
    }
    fputs("\nbinary: ", stdout);
    for (i = 0; i < size; ++i) {
        const size_t index = little ? size - i - 1 : i;
        if (i) fputc(' ', stdout);
        for (bit = 7; bit >= 0; --bit)
            fputc((bytes[index] & (1u << bit)) ? '1' : '0', stdout);
    }
    fputs("\nmemory:", stdout);
    for (i = 0; i < size; ++i)
        printf(uppercase ? " %02X" : " %02x", (unsigned)bytes[i]);
    printf("\nbyte order: %s-endian\n", little ? "little" : "big");
}

#define CS_BITS_SIGNED_FN(F, T)                                         \
    static inline void F(T value, const char *name, int uppercase)      \
    {                                                                    \
        CS_VAL();                                                        \
        printf("type: %s\nsize: %llu byte%s\nvalue: %lld\n", name,      \
               (unsigned long long)sizeof(value),                        \
               sizeof(value) == 1 ? "" : "s", (long long)value);        \
        cs_bits_dump(&value, sizeof(value), uppercase);                  \
    }
#define CS_BITS_UNSIGNED_FN(F, T)                                       \
    static inline void F(T value, const char *name, int uppercase)      \
    {                                                                    \
        CS_VAL();                                                        \
        printf("type: %s\nsize: %llu byte%s\nvalue: %llu\n", name,      \
               (unsigned long long)sizeof(value),                        \
               sizeof(value) == 1 ? "" : "s",                           \
               (unsigned long long)value);                              \
        cs_bits_dump(&value, sizeof(value), uppercase);                  \
    }

CS_BITS_SIGNED_FN(cs_bits_sc, signed char)
CS_BITS_UNSIGNED_FN(cs_bits_uc, unsigned char)
CS_BITS_SIGNED_FN(cs_bits_s, short)
CS_BITS_UNSIGNED_FN(cs_bits_us, unsigned short)
CS_BITS_SIGNED_FN(cs_bits_i, int)
CS_BITS_UNSIGNED_FN(cs_bits_ui, unsigned int)
CS_BITS_SIGNED_FN(cs_bits_l, long)
CS_BITS_UNSIGNED_FN(cs_bits_ul, unsigned long)
CS_BITS_SIGNED_FN(cs_bits_ll, long long)
CS_BITS_UNSIGNED_FN(cs_bits_ull, unsigned long long)

#undef CS_BITS_SIGNED_FN
#undef CS_BITS_UNSIGNED_FN

static inline void cs_bits_b(_Bool value, const char *name, int uppercase)
{
    CS_VAL();
    printf("type: %s\nsize: %llu byte%s\nvalue: %s\n", name,
           (unsigned long long)sizeof(value), sizeof(value) == 1 ? "" : "s",
           value ? "true" : "false");
    cs_bits_dump(&value, sizeof(value), uppercase);
}

static inline void cs_bits_c(char value, const char *name, int uppercase)
{
    CS_VAL();
    printf("type: %s\nsize: %llu byte%s\nvalue: %d", name,
           (unsigned long long)sizeof(value), sizeof(value) == 1 ? "" : "s",
           (int)value);
    if ((unsigned char)value >= 32 && (unsigned char)value < 127)
        printf(" ('%c')", value);
    fputc('\n', stdout);
    cs_bits_dump(&value, sizeof(value), uppercase);
}

static inline void cs_bits_f(float value, const char *name, int uppercase)
{
    CS_VAL();
    printf("type: %s\nsize: %llu byte%s\nvalue: %.9g\n", name,
           (unsigned long long)sizeof(value), sizeof(value) == 1 ? "" : "s",
           (double)value);
    cs_bits_dump(&value, sizeof(value), uppercase);
#if FLT_RADIX == 2 && FLT_MANT_DIG == 24 && FLT_MAX_EXP == 128
    if (sizeof(value) == sizeof(uint32_t)) {
        uint32_t raw;
        unsigned exponent;
        memcpy(&raw, &value, sizeof(raw));
        exponent = (unsigned)((raw >> 23) & UINT32_C(0xff));
        printf("sign: %u\n", (unsigned)(raw >> 31));
        if (exponent == 0)
            fputs("exponent: 0 (zero/subnormal)\n", stdout);
        else if (exponent == 0xff)
            fputs("exponent: 255 (infinity/NaN)\n", stdout);
        else
            printf("exponent: %u (%d)\n", exponent, (int)exponent - 127);
        printf(uppercase ? "fraction: 0X%06" PRIX32 "\n"
                         : "fraction: 0x%06" PRIx32 "\n",
               raw & UINT32_C(0x7fffff));
    }
#endif
}

static inline void cs_bits_d(double value, const char *name, int uppercase)
{
    CS_VAL();
    printf("type: %s\nsize: %llu byte%s\nvalue: %.17g\n", name,
           (unsigned long long)sizeof(value), sizeof(value) == 1 ? "" : "s",
           value);
    cs_bits_dump(&value, sizeof(value), uppercase);
#if FLT_RADIX == 2 && DBL_MANT_DIG == 53 && DBL_MAX_EXP == 1024
    if (sizeof(value) == sizeof(uint64_t)) {
        uint64_t raw;
        unsigned exponent;
        memcpy(&raw, &value, sizeof(raw));
        exponent = (unsigned)((raw >> 52) & UINT64_C(0x7ff));
        printf("sign: %u\n", (unsigned)(raw >> 63));
        if (exponent == 0)
            fputs("exponent: 0 (zero/subnormal)\n", stdout);
        else if (exponent == 0x7ff)
            fputs("exponent: 2047 (infinity/NaN)\n", stdout);
        else
            printf("exponent: %u (%d)\n", exponent, (int)exponent - 1023);
        printf(uppercase ? "fraction: 0X%013" PRIX64 "\n"
                         : "fraction: 0x%013" PRIx64 "\n",
               raw & UINT64_C(0xfffffffffffff));
    }
#endif
}

static inline void cs_bits_ld(long double value, const char *name, int uppercase)
{
    CS_VAL();
    printf("type: %s\nsize: %llu byte%s\nvalue: %.21Lg\n", name,
           (unsigned long long)sizeof(value), sizeof(value) == 1 ? "" : "s",
           value);
    cs_bits_dump(&value, sizeof(value), uppercase);
}

static inline void cs_bits_pointer(const void *object, size_t size,
                                   uintptr_t address, const char *name,
                                   int uppercase)
{
    CS_VAL();
    printf("type: %s\nsize: %llu byte%s\nvalue: ", name,
           (unsigned long long)size, size == 1 ? "" : "s");
    if (address)
        printf(uppercase ? "0X%" PRIXPTR "\n" : "0x%" PRIxPTR "\n",
               address);
    else
        fputs("NULL\n", stdout);
    cs_bits_dump(object, size, uppercase);
}

#define CS_BITS_POINTER_FNS(F, T)                                       \
    static inline void F(T *value, const char *name, int uppercase)     \
    {                                                                    \
        cs_bits_pointer(&value, sizeof(value), (uintptr_t)value, name,   \
                        uppercase);                                      \
    }                                                                    \
    static inline void F##_c(const T *value, const char *name,           \
                             int uppercase)                              \
    {                                                                    \
        cs_bits_pointer(&value, sizeof(value), (uintptr_t)value, name,   \
                        uppercase);                                      \
    }                                                                    \
    static inline void F##_v(volatile T *value, const char *name,        \
                             int uppercase)                              \
    {                                                                    \
        cs_bits_pointer(&value, sizeof(value), (uintptr_t)value, name,   \
                        uppercase);                                      \
    }                                                                    \
    static inline void F##_cv(const volatile T *value, const char *name, \
                              int uppercase)                             \
    {                                                                    \
        cs_bits_pointer(&value, sizeof(value), (uintptr_t)value, name,   \
                        uppercase);                                      \
    }

CS_BITS_POINTER_FNS(cs_bits_p_void, void)
CS_BITS_POINTER_FNS(cs_bits_p_b, _Bool)
CS_BITS_POINTER_FNS(cs_bits_p_c, char)
CS_BITS_POINTER_FNS(cs_bits_p_sc, signed char)
CS_BITS_POINTER_FNS(cs_bits_p_uc, unsigned char)
CS_BITS_POINTER_FNS(cs_bits_p_s, short)
CS_BITS_POINTER_FNS(cs_bits_p_us, unsigned short)
CS_BITS_POINTER_FNS(cs_bits_p_i, int)
CS_BITS_POINTER_FNS(cs_bits_p_ui, unsigned int)
CS_BITS_POINTER_FNS(cs_bits_p_l, long)
CS_BITS_POINTER_FNS(cs_bits_p_ul, unsigned long)
CS_BITS_POINTER_FNS(cs_bits_p_ll, long long)
CS_BITS_POINTER_FNS(cs_bits_p_ull, unsigned long long)
CS_BITS_POINTER_FNS(cs_bits_p_f, float)
CS_BITS_POINTER_FNS(cs_bits_p_d, double)
CS_BITS_POINTER_FNS(cs_bits_p_ld, long double)

#undef CS_BITS_POINTER_FNS
#endif

/* Struct-member formatting is deliberately different from top-level value
   formatting: a char pointer member is an address, never an implicit %s
   dereference. Generated struct printers extend the two empty association
   lists below after the user's type definitions are visible. */
static inline void cs_m_b (const volatile _Bool *v)              { printf("%s", *v ? "true" : "false"); }
static inline void cs_m_c (const volatile char *v)               { printf("%d", (int)*v); }
static inline void cs_m_sc(const volatile signed char *v)        { printf("%lld", (long long)*v); }
static inline void cs_m_uc(const volatile unsigned char *v)      { printf("%llu", (unsigned long long)*v); }
static inline void cs_m_s (const volatile short *v)              { printf("%lld", (long long)*v); }
static inline void cs_m_us(const volatile unsigned short *v)     { printf("%llu", (unsigned long long)*v); }
static inline void cs_m_i (const volatile int *v)                { printf("%lld", (long long)*v); }
static inline void cs_m_ui(const volatile unsigned int *v)       { printf("%llu", (unsigned long long)*v); }
static inline void cs_m_l (const volatile long *v)               { printf("%lld", (long long)*v); }
static inline void cs_m_ul(const volatile unsigned long *v)      { printf("%llu", (unsigned long long)*v); }
static inline void cs_m_ll(const volatile long long *v)          { printf("%lld", (long long)*v); }
static inline void cs_m_ull(const volatile unsigned long long *v){ printf("%llu", (unsigned long long)*v); }
static inline void cs_m_f (const volatile float *v)              { printf("%.17g", (double)*v); }
static inline void cs_m_d (const volatile double *v)             { printf("%.17g", *v); }
static inline void cs_m_ld(const volatile long double *v)        { printf("%.21Lg", *v); }
static inline void cs_m_unknown(const volatile void *v)          { (void)v; fputs("<unprintable>", stdout); }

#define CS_MEMBER_REF_TYPES(T, F)                                        \
    T *: F, const T *: F, volatile T *: F, const volatile T *: F
#define CS_AGG_PRINT_ASSOCIATIONS
#define CS_AGG_MEMBER_ASSOCIATIONS
#define CS_MEMBER_REF(x) _Generic(&(x),                                  \
    CS_MEMBER_REF_TYPES(_Bool, cs_m_b),                                  \
    CS_MEMBER_REF_TYPES(char, cs_m_c),                                   \
    CS_MEMBER_REF_TYPES(signed char, cs_m_sc),                           \
    CS_MEMBER_REF_TYPES(unsigned char, cs_m_uc),                         \
    CS_MEMBER_REF_TYPES(short, cs_m_s),                                  \
    CS_MEMBER_REF_TYPES(unsigned short, cs_m_us),                        \
    CS_MEMBER_REF_TYPES(int, cs_m_i),                                    \
    CS_MEMBER_REF_TYPES(unsigned int, cs_m_ui),                          \
    CS_MEMBER_REF_TYPES(long, cs_m_l),                                   \
    CS_MEMBER_REF_TYPES(unsigned long, cs_m_ul),                         \
    CS_MEMBER_REF_TYPES(long long, cs_m_ll),                             \
    CS_MEMBER_REF_TYPES(unsigned long long, cs_m_ull),                   \
    CS_MEMBER_REF_TYPES(float, cs_m_f),                                  \
    CS_MEMBER_REF_TYPES(double, cs_m_d),                                 \
    CS_MEMBER_REF_TYPES(long double, cs_m_ld),                           \
    CS_AGG_MEMBER_ASSOCIATIONS                                           \
    default: cs_m_unknown)(&(x))
#define CS_MEMBER_PTR(x) do {                                            \
    if ((x) == 0) fputs("NULL", stdout);                                \
    else printf("(void *)0x%" PRIxPTR, (uintptr_t)(x));                 \
} while (0)

#define CS_PRINT(x) _Generic((x),                                        \
    _Bool: cs_p_b,              char: cs_p_c,                            \
    signed char: cs_p_i,        unsigned char: cs_p_u,                   \
    short: cs_p_i,              unsigned short: cs_p_u,                  \
    int: cs_p_i,                unsigned int: cs_p_u,                    \
    long: cs_p_i,               unsigned long: cs_p_u,                   \
    long long: cs_p_i,          unsigned long long: cs_p_u,              \
    float: cs_p_d,              double: cs_p_d,                          \
    long double: cs_p_ld,                                                \
    char *: cs_p_s,             const char *: cs_p_s,                    \
    CS_AGG_PRINT_ASSOCIATIONS                                            \
    default: cs_p_p)(x)

/* An array decays to a pointer before _Generic can see it, so array-ness is
   settled at run time instead: an array object begins at its first element,
   while a pointer object is stored somewhere else entirely.  Both branches
   type-check for either category, so one program serves both.  The element
   count comes from sizeof, so no declarator has to be parsed. */
#define CS_ARRAY_LIMIT 100
#define CS_IS_ARRAY(x) \
    ((const volatile void *)&(x) == (const volatile void *)&(x)[0])
#define CS_ARRAY_ELIDE(n) printf("... (%llu more)", (unsigned long long)(n))

static inline void cs_p_utf8_bytes(const void *object, size_t size)
{
    const unsigned char *bytes = (const unsigned char *)object;
    size_t i, shown = size < CS_ARRAY_LIMIT ? size : CS_ARRAY_LIMIT;
    CS_VAL();
    fputs(CS_M_UTF8, stdout);
    printf("%llu:", (unsigned long long)size);
    for (i = 0; i < shown; ++i) printf("%02x", (unsigned)bytes[i]);
    putchar('\n');
}

#define CS_PRINT_UTF8_ARRAY(x) do {                                      \
    cs_p_utf8_bytes((const void *)(x), sizeof(x));                       \
} while (0)

static inline uint32_t cs_unicode_unit(const unsigned char *bytes, size_t width)
{
    if (width == 1) {
        return bytes[0];
    } else if (width == 2) {
        uint16_t value;
        memcpy(&value, bytes, sizeof(value));
        return value;
    } else {
        uint32_t value;
        memcpy(&value, bytes, sizeof(value));
        return value;
    }
}

static inline void cs_p_unicode(const void *object, size_t actual_width,
                                size_t expected_width, size_t limit, int exact)
{
    const unsigned char *bytes = (const unsigned char *)object;
    size_t i;
    char status = exact ? 'E' : 'L';

    CS_VAL();
    fputs(CS_M_UNICODE, stdout);
    printf("%llu:%" PRIxPTR ":", (unsigned long long)actual_width,
           (uintptr_t)object);
    if (!object) {
        fputs(":N:0\n", stdout);
        return;
    }
    if (actual_width != expected_width) {
        fputs(":M:0\n", stdout);
        return;
    }

    for (i = 0; i < limit; ++i) {
        uint32_t unit = cs_unicode_unit(bytes + i * actual_width, actual_width);
        if (i) fputc(',', stdout);
        printf("%08" PRIx32, unit);
        if (!exact && unit == 0) {
            status = 'T';
            ++i;
            break;
        }
    }
    printf(":%c:%llu\n", status, (unsigned long long)i);
}

#define CS_UNICODE_PTR_TYPES(T) T *: cs_p_unicode, const T *: cs_p_unicode
#define CS_PRINT_UNICODE(x, expected_width, limit, exact)                 \
    _Generic((x),                                                         \
        CS_UNICODE_PTR_TYPES(char),                                       \
        CS_UNICODE_PTR_TYPES(signed char),                                \
        CS_UNICODE_PTR_TYPES(unsigned char),                              \
        CS_UNICODE_PTR_TYPES(short),                                      \
        CS_UNICODE_PTR_TYPES(unsigned short),                             \
        CS_UNICODE_PTR_TYPES(int),                                        \
        CS_UNICODE_PTR_TYPES(unsigned int),                               \
        CS_UNICODE_PTR_TYPES(long),                                       \
        CS_UNICODE_PTR_TYPES(unsigned long),                              \
        CS_UNICODE_PTR_TYPES(long long),                                  \
        CS_UNICODE_PTR_TYPES(unsigned long long))                         \
        ((x), sizeof(*(x)), (expected_width), (limit), (exact))

#define CS_PRINT_ARRAY1(x) do {                                          \
    if (!CS_IS_ARRAY(x)) { CS_PRINT(x); break; }                         \
    CS_VAL();                                                            \
    { size_t cs_n0 = sizeof(x) / sizeof((x)[0]), cs_i0;                  \
      fputc('{', stdout);                                                \
      for (cs_i0 = 0; cs_i0 < cs_n0; ++cs_i0) {                          \
          if (cs_i0) fputs(", ", stdout);                                \
          if (cs_i0 == CS_ARRAY_LIMIT) { CS_ARRAY_ELIDE(cs_n0 - cs_i0); break; } \
          CS_MEMBER_REF((x)[cs_i0]);                                     \
      }                                                                  \
      fputc('}', stdout); }                                              \
    putchar('\n');                                                       \
} while (0)

/* Nesting depth is a static property, so it cannot be chosen at run time the
   way array-ness can.  Indexing twice does still type-check for an array of
   pointers, where the inner subscript would walk off into whatever each
   pointer happens to address, so the element gets the same run-time array
   test as the whole and falls back to the member printer when it fails. */
#define CS_PRINT_ARRAY2(x) do {                                          \
    if (!CS_IS_ARRAY(x)) { CS_PRINT(x); break; }                         \
    CS_VAL();                                                            \
    { size_t cs_n0 = sizeof(x) / sizeof((x)[0]), cs_i0;                  \
      fputc('{', stdout);                                                \
      for (cs_i0 = 0; cs_i0 < cs_n0; ++cs_i0) {                          \
          if (cs_i0) fputs(", ", stdout);                                \
          if (cs_i0 == CS_ARRAY_LIMIT) { CS_ARRAY_ELIDE(cs_n0 - cs_i0); break; } \
          if (!CS_IS_ARRAY((x)[cs_i0])) { CS_MEMBER_REF((x)[cs_i0]); continue; } \
          { size_t cs_n1 = sizeof((x)[cs_i0]) / sizeof((x)[cs_i0][0]), cs_i1; \
            fputc('{', stdout);                                          \
            for (cs_i1 = 0; cs_i1 < cs_n1; ++cs_i1) {                    \
                if (cs_i1) fputs(", ", stdout);                          \
                if (cs_i1 == CS_ARRAY_LIMIT) { CS_ARRAY_ELIDE(cs_n1 - cs_i1); break; } \
                CS_MEMBER_REF((x)[cs_i0][cs_i1]);                        \
            }                                                            \
            fputc('}', stdout); }                                        \
      }                                                                  \
      fputc('}', stdout); }                                              \
    putchar('\n');                                                       \
} while (0)

/* C has no general type reflection.  This portable _Generic table reports
   scalar types and scalar pointers.  The controlling expression is not
   evaluated; its normal lvalue/array/function conversions still apply. */
#define CS_PTR_TYPES(T, N)                                                \
    T *: N " *",                    const T *: "const " N " *",         \
    volatile T *: "volatile " N " *",                                   \
    const volatile T *: "const volatile " N " *"

#define CS_TYPE_ASSOCIATIONS                                             \
    _Bool: "_Bool",             char: "char",                           \
    signed char: "signed char", unsigned char: "unsigned char",         \
    short: "short",             unsigned short: "unsigned short",       \
    int: "int",                 unsigned int: "unsigned int",           \
    long: "long",               unsigned long: "unsigned long",         \
    long long: "long long",     unsigned long long: "unsigned long long", \
    float: "float",             double: "double",                       \
    long double: "long double",                                         \
    CS_PTR_TYPES(void, "void"),                                          \
    CS_PTR_TYPES(_Bool, "_Bool"),                                        \
    CS_PTR_TYPES(char, "char"),                                          \
    CS_PTR_TYPES(signed char, "signed char"),                            \
    CS_PTR_TYPES(unsigned char, "unsigned char"),                        \
    CS_PTR_TYPES(short, "short"),                                        \
    CS_PTR_TYPES(unsigned short, "unsigned short"),                      \
    CS_PTR_TYPES(int, "int"),                                            \
    CS_PTR_TYPES(unsigned int, "unsigned int"),                          \
    CS_PTR_TYPES(long, "long"),                                          \
    CS_PTR_TYPES(unsigned long, "unsigned long"),                        \
    CS_PTR_TYPES(long long, "long long"),                                \
    CS_PTR_TYPES(unsigned long long, "unsigned long long"),              \
    CS_PTR_TYPES(float, "float"),                                        \
    CS_PTR_TYPES(double, "double"),                                      \
    CS_PTR_TYPES(long double, "long double")

#define CS_TYPE_NAME(x) _Generic((x), CS_TYPE_ASSOCIATIONS,              \
    default: "<unrecognized type>")

#ifdef CS_ENABLE_BITS
#define CS_BITS_PTR_TYPES(T, F)                                          \
    T *: F, const T *: F##_c, volatile T *: F##_v,                      \
    const volatile T *: F##_cv

/* The first occurrence of x is _Generic's unevaluated controlling
   expression. The selected function receives x exactly once, so side effects
   happen once while the temporary parameter preserves the scalar object's
   representation for inspection. */
#define CS_BITS_WITH_CASE(x, uppercase) _Generic((x),                    \
    _Bool: cs_bits_b,             char: cs_bits_c,                       \
    signed char: cs_bits_sc,      unsigned char: cs_bits_uc,             \
    short: cs_bits_s,             unsigned short: cs_bits_us,            \
    int: cs_bits_i,               unsigned int: cs_bits_ui,              \
    long: cs_bits_l,              unsigned long: cs_bits_ul,             \
    long long: cs_bits_ll,        unsigned long long: cs_bits_ull,        \
    float: cs_bits_f,             double: cs_bits_d,                     \
    long double: cs_bits_ld,                                             \
    CS_BITS_PTR_TYPES(void, cs_bits_p_void),                             \
    CS_BITS_PTR_TYPES(_Bool, cs_bits_p_b),                               \
    CS_BITS_PTR_TYPES(char, cs_bits_p_c),                                \
    CS_BITS_PTR_TYPES(signed char, cs_bits_p_sc),                        \
    CS_BITS_PTR_TYPES(unsigned char, cs_bits_p_uc),                      \
    CS_BITS_PTR_TYPES(short, cs_bits_p_s),                               \
    CS_BITS_PTR_TYPES(unsigned short, cs_bits_p_us),                     \
    CS_BITS_PTR_TYPES(int, cs_bits_p_i),                                 \
    CS_BITS_PTR_TYPES(unsigned int, cs_bits_p_ui),                       \
    CS_BITS_PTR_TYPES(long, cs_bits_p_l),                                \
    CS_BITS_PTR_TYPES(unsigned long, cs_bits_p_ul),                      \
    CS_BITS_PTR_TYPES(long long, cs_bits_p_ll),                          \
    CS_BITS_PTR_TYPES(unsigned long long, cs_bits_p_ull),                \
    CS_BITS_PTR_TYPES(float, cs_bits_p_f),                               \
    CS_BITS_PTR_TYPES(double, cs_bits_p_d),                              \
    CS_BITS_PTR_TYPES(long double, cs_bits_p_ld))                        \
    ((x), CS_TYPE_NAME(x), (uppercase))
#define CS_BITS(x) CS_BITS_WITH_CASE((x), 0)
#define CS_BITS_UPPER(x) CS_BITS_WITH_CASE((x), 1)
#endif

#define CS_MARK(m) do { \
    fputs(m, stdout); fflush(stdout); \
    fputs(m, stderr); fflush(stderr); \
} while (0)

/* scanf is routed through a line-request marker. The parent supplies recorded
   bytes while replaying and reads one fresh terminal line only for the newest
   input. vscanf keeps format conversion and destination writes in libc. */
static int cs_taped_scanf(const char *format, ...)
{
    int result;
    va_list args;
    fputs(CS_M_STDIN, stderr);
    fflush(stderr);
    va_start(args, format);
    result = vscanf(format, args);
    va_end(args);
    return result;
}
#ifdef scanf
#undef scanf
#endif
#define scanf cs_taped_scanf

static inline uint64_t cs_timeit_now_ns(void) {
#if defined(_WIN32)
    return (uint64_t)clock() * 1000000ULL;
#elif defined(CLOCK_MONOTONIC)
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ULL + (uint64_t)ts.tv_nsec;
#else
    struct timespec ts;
    timespec_get(&ts, TIME_UTC);
    return (uint64_t)ts.tv_sec * 1000000000ULL + (uint64_t)ts.tv_nsec;
#endif
}
"##;

/// Deepest array nesting the runtime has a printer for. `CS_PRINT_ARRAY<n>`
/// must exist for every depth up to this.
pub const MAX_ARRAY_DEPTH: usize = 2;

/// What a member or element with no printer renders as. Callers use it to
/// tell "printed nothing useful" from a real value.
pub const UNPRINTABLE: &str = "<unprintable>";

/// Where a new input is spliced into the generated program.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Slot {
    /// An expression, printed via `_Generic` on its own type.
    Expr,
    /// A statement or block-scope declaration, appended inside `main`.
    Stmt,
    /// A function definition, `#include`, `#define` or type definition.
    FileScope,
}

pub struct Program {
    pub src: String,
    /// 1-based line in `src` where the user's new text starts. Diagnostics are
    /// mapped back to input-relative lines through this.
    pub new_start_line: usize,
    /// How many lines that text occupies, so diagnostics landing just past it
    /// are not misattributed to the user.
    pub new_line_count: usize,
    /// Line ranges occupied by earlier retained user inputs. Diagnostics and
    /// source excerpts outside these ranges and the new-input range are
    /// generated scaffolding and must not leak into the UI.
    pub session_line_ranges: Vec<(usize, usize)>,
    /// Whether retained/current source contains a direct `scanf` token and
    /// therefore needs the request-driven stdin tape transport.
    pub uses_stdin_tape: bool,
    /// True when the input sits inside a generated expression macro wrapper.
    /// MSVC's traditional preprocessor attributes diagnostics from a
    /// multi-line macro invocation to the invocation's *first* line — the
    /// wrapper line just above the input — so the remapper must know it may
    /// pull those anchors back into the input.
    pub wrapped: bool,
}

#[derive(Clone, Copy)]
enum BuildFlavor {
    Normal(Slot),
    SilentExpr,
    TypeProbe,
    BitsProbe(bool),
    TimeitProbe(u64),
    /// Print an expression through the array-aware printer, indexing to the
    /// given nesting depth.
    ArrayExpr(usize),
    /// Return bounded raw code units for an expression whose source spelling
    /// explicitly identifies a one-dimensional UTF-8 array.
    Utf8ArrayExpr,
    /// Decode an explicitly requested sequence of 1-, 2- or 4-byte code
    /// units. `exact` controls whether NUL terminates the scan.
    UnicodeProbe {
        expected_width: usize,
        limit: usize,
        exact: bool,
    },
    ScopedStmt,
    ReplaceFile(usize),
}

impl BuildFlavor {
    fn slot(self) -> Slot {
        match self {
            Self::Normal(slot) => slot,
            Self::SilentExpr
            | Self::TypeProbe
            | Self::BitsProbe(_)
            | Self::TimeitProbe(_)
            | Self::ArrayExpr(_)
            | Self::Utf8ArrayExpr
            | Self::UnicodeProbe { .. } => Slot::Expr,
            Self::ScopedStmt => Slot::Stmt,
            Self::ReplaceFile(_) => Slot::FileScope,
        }
    }
}

/// Render only the C program a user conceptually assembled: retained
/// file-scope inputs plus retained statements in `main`. Runtime printers and
/// protocol markers belong to `%src --raw`, not the default source view.
pub fn build_user_view(session: &Session) -> String {
    let mut src = String::new();
    for item in &session.file_items {
        src.push_str(item);
        src.push('\n');
    }
    if !session.file_items.is_empty() {
        src.push('\n');
    }
    src.push_str("int main(void)\n{\n");
    for (index, stmt) in session.stmts.iter().enumerate() {
        let events = session.stmt_stdin_event_count(index);
        if events > 0 {
            src.push_str(&format!(
                "    /* stdin tape: {events} captured request(s); contents hidden */\n"
            ));
        }
        src.push_str(stmt);
        src.push('\n');
    }
    for _ in 0..session.scope_depth {
        src.push_str("    }\n");
    }
    src.push_str("}\n");
    src
}

/// Build the program for evaluating `input` in `slot` against `session`.
pub fn build(session: &Session, input: &str, slot: Slot) -> Program {
    build_inner(session, input, BuildFlavor::Normal(slot))
}

/// Retry a block-scope declaration inside a fresh nested scope. If the real
/// compiler accepts this after rejecting the normal assembly, later inputs
/// remain in that scope and see the new binding.
pub fn build_scoped_stmt(session: &Session, input: &str) -> Program {
    build_inner(session, input, BuildFlavor::ScopedStmt)
}

/// Retry a file-scope definition by substituting it for one existing item at
/// the same index. The compiler, not a name parser, decides which candidate
/// replacement makes the whole session valid.
pub fn build_file_replacement(session: &Session, input: &str, index: usize) -> Program {
    build_inner(session, input, BuildFlavor::ReplaceFile(index))
}

/// Build a program that evaluates an expression without trying to print it.
/// Used only after the normal value-printer trial failed, to distinguish an
/// unsupported value category (struct/complex/void/etc.) from invalid C.
pub fn build_expr_probe(session: &Session, input: &str) -> Program {
    build_inner(session, input, BuildFlavor::SilentExpr)
}

/// Build a non-mutating `%type` query. `_Generic` selects a type-name string
/// without evaluating `input`; M_VAL keeps the result out of live program
/// output so the magic renderer can print it itself.
pub fn build_type_probe(session: &Session, input: &str) -> Program {
    build_inner(session, input, BuildFlavor::TypeProbe)
}

/// Build a non-mutating `%bits` query. `_Generic` selects a scalar helper
/// without evaluating its controlling expression; the selected helper then
/// receives and inspects the expression value exactly once.
pub fn build_bits_probe(session: &Session, input: &str, uppercase: bool) -> Program {
    build_inner(session, input, BuildFlavor::BitsProbe(uppercase))
}

/// Build a program that prints an expression as an array of `depth`
/// dimensions. Used only after the ordinary printer reported a bare address,
/// which is all `_Generic` can say once an array has decayed to a pointer.
/// The generated code still decides array versus pointer at run time; `depth`
/// only controls how far it indexes, and a depth deeper than the value has
/// simply fails to compile.
pub fn build_array_expr(session: &Session, input: &str, depth: usize) -> Program {
    build_inner(session, input, BuildFlavor::ArrayExpr(depth))
}

/// Build a bounded raw-code-unit probe for an expression that Rust has
/// conservatively identified as an explicit `char8_t[]` or `u8"..."` array.
pub fn build_utf8_array_expr(session: &Session, input: &str) -> Program {
    build_inner(session, input, BuildFlavor::Utf8ArrayExpr)
}

/// Build a non-mutating, single-evaluation Unicode code-unit probe.
pub fn build_unicode_probe(
    session: &Session,
    input: &str,
    expected_width: usize,
    limit: usize,
    exact: bool,
) -> Program {
    build_inner(
        session,
        input,
        BuildFlavor::UnicodeProbe {
            expected_width,
            limit,
            exact,
        },
    )
}

/// Build a program that benchmarks an expression or statement over `loops` iterations.
pub fn build_timeit_probe(session: &Session, input: &str, loops: u64) -> Program {
    build_inner(session, input, BuildFlavor::TimeitProbe(loops))
}

fn build_inner(session: &Session, input: &str, flavor: BuildFlavor) -> Program {
    let slot = flavor.slot();
    let silent_expr = matches!(flavor, BuildFlavor::SilentExpr);
    let type_probe = matches!(flavor, BuildFlavor::TypeProbe);
    let bits_uppercase = match flavor {
        BuildFlavor::BitsProbe(uppercase) => Some(uppercase),
        _ => None,
    };
    let scoped_stmt = matches!(flavor, BuildFlavor::ScopedStmt);
    let array_depth = match flavor {
        BuildFlavor::ArrayExpr(depth) => Some(depth),
        _ => None,
    };
    let utf8_array = matches!(flavor, BuildFlavor::Utf8ArrayExpr);
    let unicode_probe = match flavor {
        BuildFlavor::UnicodeProbe {
            expected_width,
            limit,
            exact,
        } => Some((expected_width, limit, exact)),
        _ => None,
    };
    let timeit_loops = match flavor {
        BuildFlavor::TimeitProbe(loops) => Some(loops),
        _ => None,
    };
    let file_replacement = match flavor {
        BuildFlavor::ReplaceFile(index) => Some(index),
        _ => None,
    };
    let uses_stdin_tape = session
        .file_items
        .iter()
        .chain(session.stmts.iter())
        .map(String::as_str)
        .chain(std::iter::once(input))
        .any(|source| lex::contains_code_identifier(source, "scanf"));
    let mut src = String::with_capacity(4096);
    let mut new_start_line = 1usize;
    let mut session_line_ranges = Vec::new();

    src.push_str(HEADERS);
    // Ahead of the runtime, which expands them.
    src.push_str(&format!("#define CS_M_NEW \"{}\"\n", escape(M_NEW)));
    src.push_str(&format!("#define CS_M_VAL \"{}\"\n", escape(M_VAL)));
    src.push_str(&format!("#define CS_M_DONE \"{}\"\n", escape(M_DONE)));
    src.push_str(&format!("#define CS_M_STDIN \"{}\"\n", escape(M_STDIN)));
    src.push_str(&format!("#define CS_M_UTF8 \"{}\"\n", escape(M_UTF8)));
    src.push_str(&format!("#define CS_M_UNICODE \"{}\"\n", escape(M_UNICODE)));
    if bits_uppercase.is_some() {
        src.push_str("#define CS_ENABLE_BITS 1\n");
    }
    src.push_str(RUNTIME);

    for (index, item) in session.file_items.iter().enumerate() {
        if file_replacement == Some(index) {
            new_start_line = src.lines().count() + 1;
            src.push_str(input);
        } else {
            let start = src.lines().count() + 1;
            src.push_str(item);
            session_line_ranges.push((start, item.lines().count().max(1)));
        }
        src.push('\n');
    }

    if slot == Slot::FileScope && file_replacement.is_none() {
        new_start_line = src.lines().count() + 1;
        src.push_str(input);
        src.push('\n');
    }

    let aggregate_printers = build_aggregate_printers(&src);
    src.push_str(&aggregate_printers);
    src.push_str("\nint main(void)\n{\n");
    for stmt in &session.stmts {
        let start = src.lines().count() + 1;
        src.push_str(stmt);
        session_line_ranges.push((start, stmt.lines().count().max(1)));
        src.push('\n');
    }
    src.push_str("    CS_MARK(CS_M_NEW);\n");

    match slot {
        Slot::FileScope => {}
        Slot::Stmt => {
            if scoped_stmt {
                src.push_str("    {\n");
            }
            new_start_line = src.lines().count() + 1;
            src.push_str(input);
            src.push('\n');
        }
        Slot::Expr => {
            // The input sits on a line of its own so a diagnostic inside it
            // lands on text the user actually typed, at the column they see.
            new_start_line = src.lines().count() + if type_probe { 3 } else { 2 };
            if type_probe {
                let aggregates = aggregate_types(session, input);
                src.push_str("    CS_VAL();\n");
                if aggregates.is_empty() {
                    src.push_str(&format!("    puts(CS_TYPE_NAME((\n{input}\n    )));\n"));
                } else {
                    src.push_str(&format!("    puts(_Generic((\n{input}\n    ),\n"));
                    src.push_str("        CS_TYPE_ASSOCIATIONS,\n");
                    for (ty, category) in aggregates {
                        let name = ty
                            .strip_prefix("struct ")
                            .or_else(|| ty.strip_prefix("union "))
                            .unwrap_or(&ty);
                        src.push_str(&format!("        {ty}: \"{category} {name}\",\n"));
                    }
                    src.push_str("        default: \"<unrecognized type>\"));\n");
                }
            } else if let Some(uppercase) = bits_uppercase {
                let macro_name = if uppercase {
                    "CS_BITS_UPPER"
                } else {
                    "CS_BITS"
                };
                src.push_str(&format!("    {macro_name}((\n{input}\n    ));\n"));
            } else if let Some(loops) = timeit_loops {
                src.push_str("    CS_VAL();\n");
                src.push_str("    {\n");
                src.push_str(&format!("        uint64_t _cs_loops = {loops}ULL;\n"));
                src.push_str("        uint64_t _cs_start = cs_timeit_now_ns();\n");
                src.push_str("        for (uint64_t _cs_i = 0; _cs_i < _cs_loops; ++_cs_i) {\n");
                if crate::eval::should_try_expr(input) {
                    src.push_str(&format!("            (void)(\n{input}\n            );\n"));
                } else {
                    src.push_str(&format!("            {{\n{input}\n            }}\n"));
                }
                src.push_str("        }\n");
                src.push_str("        uint64_t _cs_end = cs_timeit_now_ns();\n");
                src.push_str(
                    "        printf(\"%llu\\n\", (unsigned long long)(_cs_end - _cs_start));\n",
                );
                src.push_str("    }\n");
            } else if silent_expr {
                src.push_str(&format!("    (void)(\n{input}\n    );\n"));
            } else if utf8_array {
                src.push_str(&format!("    CS_PRINT_UTF8_ARRAY((\n{input}\n    ));\n"));
            } else if let Some((expected_width, limit, exact)) = unicode_probe {
                src.push_str(&format!(
                    "    CS_PRINT_UNICODE((\n{input}\n    ), {expected_width}, {limit}, {});\n",
                    usize::from(exact)
                ));
            } else if let Some(depth) = array_depth {
                src.push_str(&format!("    CS_PRINT_ARRAY{depth}((\n{input}\n    ));\n"));
            } else {
                src.push_str(&format!("    CS_PRINT((\n{input}\n    ));\n"));
            }
        }
    }

    src.push_str("    CS_MARK(CS_M_DONE);\n");
    for _ in 0..session.scope_depth + usize::from(scoped_stmt) {
        src.push_str("    }\n");
    }
    src.push_str("    return 0;\n}\n");
    let new_line_count = input.lines().count().max(1);
    Program {
        src,
        new_start_line,
        new_line_count,
        session_line_ranges,
        uses_stdin_tape,
        wrapped: slot == Slot::Expr && !silent_expr && timeit_loops.is_none(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum AggregateToken {
    Ident(String),
    Punct(char),
    Directive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AggregateMemberKind {
    Value,
    Pointer,
    Array {
        dimensions: usize,
        pointer_elements: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AggregateMember {
    name: String,
    kind: AggregateMemberKind,
    type_hint: Option<String>,
    visibly_nested: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AggregatePrinter {
    ty: String,
    members: Vec<AggregateMember>,
    byte_fallback: bool,
}

/// Build safe, session-specific struct printers after the user's type
/// definitions are visible. Pointers are never dereferenced. Declarators that
/// are easy to misidentify (multi-declarators, function pointers, bit-fields
/// and anonymous members) select an explicit raw-byte fallback instead.
fn build_aggregate_printers(source: &str) -> String {
    let printers = collect_aggregate_printers(source);
    if printers.is_empty() {
        return String::new();
    }

    let mut out = String::from("\n/* c-shell aggregate printers */\n");
    out.push_str("static unsigned cs_aggregate_depth;\n");
    out.push_str(
        "static void cs_aggregate_indent(void) { unsigned i; for (i = 0; i < cs_aggregate_depth; ++i) fputs(\"    \", stdout); }\n",
    );
    for (index, printer) in printers.iter().enumerate() {
        out.push_str(&format!(
            "static void cs_m_ag_{index}(const volatile {} *);\n",
            printer.ty
        ));
        out.push_str(&format!("static void cs_p_ag_{index}({});\n", printer.ty));
    }

    out.push_str("#undef CS_AGG_PRINT_ASSOCIATIONS\n");
    out.push_str("#define CS_AGG_PRINT_ASSOCIATIONS ");
    for (index, printer) in printers.iter().enumerate() {
        out.push_str(&format!("{}: cs_p_ag_{index}, ", printer.ty));
    }
    out.push('\n');
    out.push_str("#undef CS_AGG_MEMBER_ASSOCIATIONS\n");
    out.push_str("#define CS_AGG_MEMBER_ASSOCIATIONS ");
    for (index, printer) in printers.iter().enumerate() {
        out.push_str(&format!(
            "{} *: cs_m_ag_{index}, const {} *: cs_m_ag_{index}, volatile {} *: cs_m_ag_{index}, const volatile {} *: cs_m_ag_{index}, ",
            printer.ty, printer.ty, printer.ty, printer.ty
        ));
    }
    out.push('\n');

    for (index, printer) in printers.iter().enumerate() {
        if printer.byte_fallback {
            emit_byte_fallback(&mut out, index, printer);
        } else {
            emit_struct_formatter(&mut out, index, printer);
        }
        out.push_str(&format!(
            "static void cs_p_ag_{index}({ty} v) {{ CS_VAL(); cs_aggregate_depth = 0; cs_m_ag_{index}(&v); putchar('\\n'); }}\n",
            ty = printer.ty
        ));
    }
    out
}

fn emit_byte_fallback(out: &mut String, index: usize, printer: &AggregatePrinter) {
    let label = escape(&printer.ty);
    out.push_str(&format!(
        "static void cs_m_ag_{index}(const volatile {ty} *v) {{\n\
         const volatile unsigned char *p = (const volatile unsigned char *)v;\n\
         size_t i; fputs(\"<{label} raw bytes:\", stdout);\n\
         for (i = 0; i < sizeof(*v); ++i) printf(\" %02x\", (unsigned)p[i]);\n\
         fputc('>', stdout);\n\
         }}\n",
        ty = printer.ty
    ));
}

fn emit_struct_formatter(out: &mut String, index: usize, printer: &AggregatePrinter) {
    let multiline = printer.members.len() > 4
        || printer.members.iter().any(|member| {
            member.visibly_nested || matches!(member.kind, AggregateMemberKind::Array { .. })
        });
    out.push_str(&format!(
        "static void cs_m_ag_{index}(const volatile {} *v) {{\n",
        printer.ty
    ));
    if multiline {
        out.push_str("fputs(\"{\\n\", stdout); ++cs_aggregate_depth;\n");
    } else {
        out.push_str("fputs(\"{ \", stdout);\n");
    }
    for (member_index, member) in printer.members.iter().enumerate() {
        if multiline {
            if member_index > 0 {
                out.push_str("fputs(\",\\n\", stdout);\n");
            }
            out.push_str("cs_aggregate_indent();\n");
        } else if member_index > 0 {
            out.push_str("fputs(\", \", stdout);\n");
        }
        out.push_str(&format!("fputs(\".{} = \", stdout);\n", member.name));
        let expr = format!("v->{}", member.name);
        match member.kind {
            AggregateMemberKind::Value => {
                out.push_str(&format!("CS_MEMBER_REF({expr});\n"));
            }
            AggregateMemberKind::Pointer => {
                out.push_str(&format!("CS_MEMBER_PTR({expr});\n"));
            }
            AggregateMemberKind::Array {
                dimensions,
                pointer_elements,
            } => emit_array_formatter(out, &expr, dimensions, pointer_elements, member_index, 0),
        }
    }
    if multiline {
        out.push_str("--cs_aggregate_depth; fputc('\\n', stdout); cs_aggregate_indent(); fputc('}', stdout);\n");
    } else {
        out.push_str("fputs(\" }\", stdout);\n");
    }
    out.push_str("}\n");
}

fn emit_array_formatter(
    out: &mut String,
    expr: &str,
    dimensions: usize,
    pointer_elements: bool,
    member_index: usize,
    level: usize,
) {
    let index = format!("cs_i_{member_index}_{level}");
    let count = format!("cs_n_{member_index}_{level}");
    out.push_str("{ ");
    out.push_str(&format!(
        "size_t {count} = sizeof({expr}) / sizeof(({expr})[0]); size_t {index}; fputc('{{', stdout); "
    ));
    out.push_str(&format!(
        "for ({index} = 0; {index} < {count}; ++{index}) {{ if ({index}) fputs(\", \", stdout); \
         if ({index} == CS_ARRAY_LIMIT) {{ CS_ARRAY_ELIDE({count} - {index}); break; }} "
    ));
    let element = format!("({expr})[{index}]");
    if level + 1 < dimensions {
        emit_array_formatter(
            out,
            &element,
            dimensions,
            pointer_elements,
            member_index,
            level + 1,
        );
    } else if pointer_elements {
        out.push_str(&format!("CS_MEMBER_PTR({element}); "));
    } else {
        out.push_str(&format!("CS_MEMBER_REF({element}); "));
    }
    out.push_str("} fputc('}', stdout); }\n");
}

fn collect_aggregate_printers(source: &str) -> Vec<AggregatePrinter> {
    let tokens = aggregate_tokens(source);
    let mut printers = Vec::new();
    let mut seen = BTreeSet::new();
    let mut brace_depth = 0usize;

    for (i, token) in tokens.iter().enumerate() {
        let AggregateToken::Ident(kind) = token else {
            match token {
                AggregateToken::Punct('{') => brace_depth += 1,
                AggregateToken::Punct('}') => brace_depth = brace_depth.saturating_sub(1),
                _ => {}
            }
            continue;
        };
        if brace_depth != 0 || !matches!(kind.as_str(), "struct" | "union") {
            continue;
        }

        let (tag, open) = match (tokens.get(i + 1), tokens.get(i + 2)) {
            (Some(AggregateToken::Ident(tag)), Some(AggregateToken::Punct('{'))) => {
                (Some(tag.clone()), i + 2)
            }
            (Some(AggregateToken::Punct('{')), _) => (None, i + 1),
            _ => continue,
        };
        let Some(close) = matching_brace(&tokens, open) else {
            continue;
        };
        let ty = if let Some(tag) = tag {
            format!("{kind} {tag}")
        } else {
            let preceded_by_typedef = i
                .checked_sub(1)
                .and_then(|j| tokens.get(j))
                .is_some_and(|t| matches!(t, AggregateToken::Ident(word) if word == "typedef"));
            if !preceded_by_typedef {
                continue;
            }
            match (tokens.get(close + 1), tokens.get(close + 2)) {
                (Some(AggregateToken::Ident(alias)), Some(AggregateToken::Punct(';'))) => {
                    alias.clone()
                }
                _ => continue,
            }
        };
        if !seen.insert(ty.clone()) {
            continue;
        }

        let members = parse_aggregate_members(&tokens[open + 1..close]);
        let (members, byte_fallback) = match members {
            Ok(members) if kind == "struct" && !members.is_empty() => (members, false),
            _ => (Vec::new(), true),
        };
        printers.push(AggregatePrinter {
            ty,
            members,
            byte_fallback,
        });
    }
    let known: BTreeSet<String> = printers.iter().map(|printer| printer.ty.clone()).collect();
    for printer in &mut printers {
        for member in &mut printer.members {
            member.visibly_nested |= matches!(member.kind, AggregateMemberKind::Value)
                && member
                    .type_hint
                    .as_ref()
                    .is_some_and(|hint| known.contains(hint));
        }
    }
    printers
}

fn matching_brace(tokens: &[AggregateToken], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (i, token) in tokens.iter().enumerate().skip(open) {
        match token {
            AggregateToken::Punct('{') => depth += 1,
            AggregateToken::Punct('}') => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

fn parse_aggregate_members(tokens: &[AggregateToken]) -> Result<Vec<AggregateMember>, ()> {
    if tokens
        .iter()
        .any(|token| matches!(token, AggregateToken::Directive))
    {
        return Err(());
    }
    let mut members = Vec::new();
    let mut start = 0usize;
    let (mut braces, mut parens, mut brackets) = (0usize, 0usize, 0usize);
    for (i, token) in tokens.iter().enumerate() {
        match token {
            AggregateToken::Punct('{') => braces += 1,
            AggregateToken::Punct('}') => braces = braces.saturating_sub(1),
            AggregateToken::Punct('(') => parens += 1,
            AggregateToken::Punct(')') => parens = parens.saturating_sub(1),
            AggregateToken::Punct('[') => brackets += 1,
            AggregateToken::Punct(']') => brackets = brackets.saturating_sub(1),
            AggregateToken::Punct(';') if braces == 0 && parens == 0 && brackets == 0 => {
                let decl = &tokens[start..i];
                if !decl.is_empty()
                    && !matches!(decl.first(), Some(AggregateToken::Ident(word)) if word == "_Static_assert" || word == "static_assert")
                {
                    members.push(parse_aggregate_member(decl)?);
                }
                start = i + 1;
            }
            _ => {}
        }
    }
    if tokens[start..]
        .iter()
        .any(|token| !matches!(token, AggregateToken::Punct(';')))
    {
        return Err(());
    }
    Ok(members)
}

fn parse_aggregate_member(tokens: &[AggregateToken]) -> Result<AggregateMember, ()> {
    let mut top_identifiers = Vec::new();
    let (mut braces, mut parens, mut brackets) = (0usize, 0usize, 0usize);
    let mut stars = 0usize;
    let mut dimensions = 0usize;
    let mut flexible = false;
    let mut inline_aggregate = false;
    let mut saw_parentheses = false;

    for (i, token) in tokens.iter().enumerate() {
        match token {
            AggregateToken::Punct('{') => {
                if braces == 0 && parens == 0 && brackets == 0 {
                    inline_aggregate = true;
                }
                braces += 1;
            }
            AggregateToken::Punct('}') => braces = braces.saturating_sub(1),
            AggregateToken::Punct('(') => {
                if braces == 0 && brackets == 0 {
                    saw_parentheses = true;
                }
                parens += 1;
            }
            AggregateToken::Punct(')') => parens = parens.saturating_sub(1),
            AggregateToken::Punct('[') if braces == 0 && parens == 0 && brackets == 0 => {
                dimensions += 1;
                flexible |= matches!(tokens.get(i + 1), Some(AggregateToken::Punct(']')));
                brackets += 1;
            }
            AggregateToken::Punct('[') => brackets += 1,
            AggregateToken::Punct(']') => brackets = brackets.saturating_sub(1),
            AggregateToken::Punct(',') if braces == 0 && parens == 0 && brackets == 0 => {
                // Multi-declarators are intentionally conservative until a C
                // declarator parser replaces this lexical extractor.
                return Err(());
            }
            AggregateToken::Punct(':') if braces == 0 && parens == 0 && brackets == 0 => {
                return Err(());
            }
            AggregateToken::Punct('*') if braces == 0 && brackets == 0 => {
                if parens > 0 {
                    // `int (*fp)(void)`: do not guess a name hidden in a
                    // parenthesized declarator.
                    return Err(());
                }
                stars += 1;
            }
            AggregateToken::Ident(word) if braces == 0 && parens == 0 && brackets == 0 => {
                if matches!(word.as_str(), "__attribute__" | "__declspec") {
                    return Err(());
                }
                top_identifiers.push((i, word.clone()));
            }
            _ => {}
        }
    }
    if flexible || top_identifiers.len() < 2 {
        return Err(());
    }
    let (_, name) = top_identifiers.last().ok_or(())?;
    if is_type_word(name) {
        return Err(());
    }
    let first = top_identifiers.first().map(|(_, word)| word.as_str());
    let type_prefix = &top_identifiers[..top_identifiers.len() - 1];
    let has_tag_keyword = type_prefix
        .iter()
        .any(|(_, word)| matches!(word.as_str(), "struct" | "union" | "enum"));
    let unknown_type_words = type_prefix
        .iter()
        .filter(|(_, word)| !is_type_word(word))
        .count();
    let mixes_typedef_with_builtin = unknown_type_words > 0
        && type_prefix
            .iter()
            .any(|(_, word)| is_concrete_type_word(word));
    if !has_tag_keyword && (unknown_type_words > 1 || mixes_typedef_with_builtin) {
        // A member macro can otherwise masquerade as an extra type word and
        // make us silently omit fields introduced by its expansion.
        return Err(());
    }
    if saw_parentheses && !matches!(first, Some("_Atomic" | "_Alignas" | "alignas")) {
        // Function-like member macros and parenthesized declarators are not
        // guessed. `_Atomic(T)` and alignment specifiers still leave a plain
        // member name at top level.
        return Err(());
    }
    if matches!(first, Some("struct" | "union" | "enum"))
        && top_identifiers.len() == 2
        && !inline_aggregate
    {
        return Err(());
    }
    if inline_aggregate && stars == 0 {
        // C11 anonymous/by-value aggregate members need their own recursive
        // declarator model. Fall back rather than silently omitting fields.
        return Err(());
    }

    let visibly_nested = stars == 0 && matches!(first, Some("struct" | "union"));
    let type_hint = if matches!(first, Some("struct" | "union")) {
        top_identifiers
            .get(1)
            .map(|(_, tag)| format!("{} {tag}", first.expect("matched aggregate keyword")))
    } else {
        top_identifiers
            .iter()
            .rev()
            .skip(1)
            .find(|(_, word)| !is_type_qualifier(word))
            .map(|(_, word)| word.clone())
    };
    let kind = if dimensions > 0 {
        AggregateMemberKind::Array {
            dimensions,
            pointer_elements: stars > 0,
        }
    } else if stars > 0 {
        AggregateMemberKind::Pointer
    } else {
        AggregateMemberKind::Value
    };
    Ok(AggregateMember {
        name: name.clone(),
        kind,
        type_hint,
        visibly_nested,
    })
}

fn is_type_qualifier(word: &str) -> bool {
    matches!(word, "const" | "volatile" | "restrict" | "_Atomic")
}

fn is_concrete_type_word(word: &str) -> bool {
    matches!(
        word,
        "void"
            | "_Bool"
            | "char"
            | "short"
            | "int"
            | "long"
            | "float"
            | "double"
            | "signed"
            | "unsigned"
            | "_Complex"
    )
}

fn is_type_word(word: &str) -> bool {
    is_concrete_type_word(word)
        || matches!(
            word,
            "const"
                | "volatile"
                | "restrict"
                | "_Atomic"
                | "_Alignas"
                | "alignas"
                | "struct"
                | "union"
                | "enum"
        )
}

fn aggregate_tokens(src: &str) -> Vec<AggregateToken> {
    let scan = lex::scan(src);
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut at_line_start = true;
    let mut in_directive = false;
    let mut conditional_depth = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            let continued = i > 0 && bytes[i - 1] == b'\\';
            if !continued {
                in_directive = false;
            }
            at_line_start = !in_directive;
            i += 1;
            continue;
        }
        if at_line_start && bytes[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if at_line_start && scan.code[i] && bytes[i] == b'#' {
            let end = bytes[i..]
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(bytes.len(), |offset| i + offset);
            let directive = src[i + 1..end].trim_start();
            let word = directive
                .split(|c: char| c.is_ascii_whitespace())
                .next()
                .unwrap_or("");
            match word {
                "if" | "ifdef" | "ifndef" => conditional_depth += 1,
                "endif" => conditional_depth = conditional_depth.saturating_sub(1),
                _ => {}
            }
            out.push(AggregateToken::Directive);
            in_directive = true;
            at_line_start = false;
        } else if at_line_start && scan.code[i] {
            at_line_start = false;
        }
        if in_directive || conditional_depth > 0 || !scan.code[i] || bytes[i].is_ascii_whitespace()
        {
            i += 1;
            continue;
        }
        if bytes[i] == b'_' || bytes[i].is_ascii_alphabetic() {
            let start = i;
            i += 1;
            while i < bytes.len()
                && scan.code[i]
                && (bytes[i] == b'_' || bytes[i].is_ascii_alphanumeric())
            {
                i += 1;
            }
            out.push(AggregateToken::Ident(src[start..i].to_string()));
            continue;
        }
        out.push(AggregateToken::Punct(bytes[i] as char));
        i += 1;
    }
    out
}

#[derive(Debug, PartialEq, Eq)]
enum TypeToken {
    Ident(String),
    LBrace,
    RBrace,
    Semi,
    Other,
}

/// Collect aggregate types that have a reusable spelling. `_Generic` has no
/// wildcard for "any struct", so each complete named tag (plus the common
/// anonymous-typedef form) is added to this query's association list.
fn aggregate_types(session: &Session, input: &str) -> Vec<(String, &'static str)> {
    let mut found = BTreeSet::new();
    for src in session
        .file_items
        .iter()
        .chain(session.stmts.iter())
        .map(String::as_str)
        .chain(std::iter::once(input))
    {
        collect_aggregate_types(src, &mut found);
    }
    found.into_iter().collect()
}

fn collect_aggregate_types(src: &str, found: &mut BTreeSet<(String, &'static str)>) {
    let tokens = type_tokens(src);
    for (i, token) in tokens.iter().enumerate() {
        let TypeToken::Ident(keyword) = token else {
            continue;
        };
        let category = match keyword.as_str() {
            "struct" => "Struct",
            "union" => "Union",
            _ => continue,
        };

        // `struct Tag { ... }`: the tag is complete and can legally appear as
        // a generic association type. Mere forward declarations are skipped.
        if let (Some(TypeToken::Ident(name)), Some(TypeToken::LBrace)) =
            (tokens.get(i + 1), tokens.get(i + 2))
        {
            found.insert((format!("{keyword} {name}"), category));
            continue;
        }

        // `typedef struct { ... } Name;`: anonymous aggregates have no tag,
        // but this simple and overwhelmingly common typedef gives us a stable
        // spelling. More elaborate multi-declarator typedefs are left unknown
        // rather than guessed incorrectly.
        let preceded_by_typedef = i
            .checked_sub(1)
            .and_then(|j| tokens.get(j))
            .is_some_and(|t| matches!(t, TypeToken::Ident(word) if word == "typedef"));
        if !preceded_by_typedef || tokens.get(i + 1) != Some(&TypeToken::LBrace) {
            continue;
        }
        let mut depth = 0usize;
        let mut close = None;
        for (j, t) in tokens.iter().enumerate().skip(i + 1) {
            match t {
                TypeToken::LBrace => depth += 1,
                TypeToken::RBrace => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(j);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(close) = close else { continue };
        if let (Some(TypeToken::Ident(alias)), Some(TypeToken::Semi)) =
            (tokens.get(close + 1), tokens.get(close + 2))
        {
            found.insert((alias.clone(), category));
        }
    }
}

fn type_tokens(src: &str) -> Vec<TypeToken> {
    let sc = lex::scan(src);
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if !sc.code[i] {
            i += 1;
            continue;
        }
        if b[i] == b'_' || b[i].is_ascii_alphabetic() {
            let start = i;
            i += 1;
            while i < b.len() && sc.code[i] && (b[i] == b'_' || b[i].is_ascii_alphanumeric()) {
                i += 1;
            }
            out.push(TypeToken::Ident(src[start..i].to_string()));
            continue;
        }
        match b[i] {
            b'{' => out.push(TypeToken::LBrace),
            b'}' => out.push(TypeToken::RBrace),
            b';' => out.push(TypeToken::Semi),
            c if !c.is_ascii_whitespace() => out.push(TypeToken::Other),
            _ => {}
        }
        i += 1;
    }
    out
}

fn escape(s: &str) -> String {
    s.bytes().map(|b| format!("\\x{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uchar_is_guarded_for_c_libraries_that_do_not_ship_it() {
        let condition = HEADERS
            .find("#if defined(__has_include)")
            .expect("optional-header guard");
        let probe = HEADERS
            .find("#  if __has_include(<uchar.h>)")
            .expect("uchar availability probe");
        let include = HEADERS
            .find("#include <uchar.h>")
            .expect("guarded uchar include");
        let end = HEADERS[include..]
            .find("#  endif")
            .map(|offset| include + offset)
            .expect("end of uchar availability branch");
        assert!(condition < probe && probe < include && include < end);
        assert!(HEADERS.contains("#include <wchar.h>"));
    }

    #[test]
    fn finds_reusable_struct_and_union_type_spellings() {
        let mut found = BTreeSet::new();
        collect_aggregate_types(
            "struct Point { int x; int y; };\n\
             union Value { int i; double d; };\n\
             typedef struct { int code; } Result;\n\
             typedef union { int code; } *ResultPtr;\n\
             struct Forward; struct Forward *ptr;\n\
             /* union Fake { int no; }; */",
            &mut found,
        );
        assert!(found.contains(&("struct Point".into(), "Struct")));
        assert!(found.contains(&("union Value".into(), "Union")));
        assert!(found.contains(&("Result".into(), "Struct")));
        assert!(!found.iter().any(|(ty, _)| ty.contains("ResultPtr")));
        assert!(!found.iter().any(|(ty, _)| ty.contains("Forward")));
        assert!(!found.iter().any(|(ty, _)| ty.contains("Fake")));
    }

    #[test]
    fn type_probe_adds_session_aggregate_associations() {
        let mut session = Session::default();
        session.file_items.push("struct Pair { int x, y; };".into());
        session.stmts.push("struct Pair pair = { 1, 2 };".into());
        let program = build_type_probe(&session, "pair");
        assert!(program.src.contains("struct Pair: \"Struct Pair\""));
    }

    #[test]
    fn bits_probe_uses_the_single_evaluation_wrapper() {
        let program = build_bits_probe(&Session::default(), "counter++", false);
        assert!(program.src.contains("CS_BITS((\ncounter++\n    ));"));
        assert_eq!(
            program.src.matches("counter++").count(),
            1,
            "code generation must not duplicate a side-effecting expression"
        );
        assert!(program.wrapped);

        let uppercase = build_bits_probe(&Session::default(), "value", true);
        assert!(uppercase.src.contains("CS_BITS_UPPER((\nvalue\n    ));"));
    }

    #[test]
    fn runtime_backs_every_advertised_array_constant() {
        assert!(
            RUNTIME.contains(&format!("fputs(\"{UNPRINTABLE}\", stdout)")),
            "the member fallback no longer prints UNPRINTABLE"
        );
        for depth in 1..=MAX_ARRAY_DEPTH {
            assert!(
                RUNTIME.contains(&format!("#define CS_PRINT_ARRAY{depth}(")),
                "no runtime printer for array depth {depth}"
            );
        }
        assert!(
            !RUNTIME.contains(&format!("#define CS_PRINT_ARRAY{}(", MAX_ARRAY_DEPTH + 1)),
            "a deeper printer exists but MAX_ARRAY_DEPTH does not reach it"
        );
    }

    #[test]
    fn array_expression_builds_select_the_matching_depth_wrapper() {
        let session = Session::default();
        let one = build_array_expr(&session, "values", 1);
        assert!(one.src.contains("CS_PRINT_ARRAY1((\nvalues\n    ));"));
        assert!(one.wrapped, "the array wrapper must remap like CS_PRINT");
        let two = build_array_expr(&session, "grid", 2);
        assert!(two.src.contains("CS_PRINT_ARRAY2((\ngrid\n    ));"));
    }

    #[test]
    fn utf8_array_expression_uses_the_bounded_raw_probe() {
        let program = build_utf8_array_expr(&Session::default(), "text");
        assert!(program.src.contains("CS_PRINT_UTF8_ARRAY((\ntext\n    ));"));
        assert!(program.src.contains("#define CS_M_UTF8"));
        let utf8_macro = program
            .src
            .split_once("#define CS_PRINT_UTF8_ARRAY")
            .expect("UTF-8 macro")
            .1
            .split_once("static inline uint32_t cs_unicode_unit")
            .expect("following Unicode helper")
            .0;
        assert!(!utf8_macro.contains("CS_IS_ARRAY"));
        assert!(program.wrapped);
    }

    #[test]
    fn unicode_probe_evaluates_the_pointer_expression_once() {
        let expression = "choose_pointer(++calls)";
        let program = build_unicode_probe(&Session::default(), expression, 2, 17, true);
        assert!(
            program
                .src
                .contains("CS_PRINT_UNICODE((\nchoose_pointer(++calls)\n    ), 2, 17, 1);")
        );
        assert_eq!(
            program.src.matches(expression).count(),
            1,
            "generated source duplicated a side-effecting pointer expression"
        );
        assert!(program.src.contains("#define CS_M_UNICODE"));
        assert!(program.wrapped);
    }

    #[test]
    fn aggregate_printers_are_safe_and_conservative_about_declarators() {
        let printers = collect_aggregate_printers(
            "#define FAKE_TYPE struct Fake { int hidden; }\n\
             #define FIELDS int injected;\n\
             #if 0\nstruct Conditional { int hidden; };\n#endif\n\
             struct Person { char *name; int age; int scores[3]; struct Person *next; };\n\
             struct Pair { int x, y; };\n\
             struct MacroFields { FIELDS int visible; };\n\
             struct Hooks { int (*callback)(void); int enabled; };\n\
             struct Anonymous { struct { int value; }; int other; };",
        );
        assert!(
            printers.iter().all(|printer| {
                printer.ty != "struct Fake" && printer.ty != "struct Conditional"
            }),
            "preprocessor-controlled text is not a stable visible type definition"
        );
        let person = printers
            .iter()
            .find(|printer| printer.ty == "struct Person")
            .expect("Person printer");
        assert!(!person.byte_fallback);
        assert_eq!(
            person
                .members
                .iter()
                .map(|member| (member.name.as_str(), member.kind))
                .collect::<Vec<_>>(),
            vec![
                ("name", AggregateMemberKind::Pointer),
                ("age", AggregateMemberKind::Value),
                (
                    "scores",
                    AggregateMemberKind::Array {
                        dimensions: 1,
                        pointer_elements: false
                    }
                ),
                ("next", AggregateMemberKind::Pointer),
            ]
        );
        for ty in [
            "struct Pair",
            "struct MacroFields",
            "struct Hooks",
            "struct Anonymous",
        ] {
            assert!(
                printers
                    .iter()
                    .find(|printer| printer.ty == ty)
                    .is_some_and(|printer| printer.byte_fallback),
                "{ty} should use the conservative byte fallback"
            );
        }

        let generated = build_aggregate_printers(
            "struct Child { int value; }; struct Parent { struct Child child; char *text; };",
        );
        assert!(generated.contains("CS_AGG_MEMBER_ASSOCIATIONS"));
        assert!(generated.contains("CS_MEMBER_PTR(v->text)"));
        assert!(!generated.contains("CS_PRINT(v->text)"));
    }

    #[test]
    fn user_source_view_omits_runtime_scaffolding() {
        let mut session = Session::default();
        session.commit("int twice(int x) { return x * 2; }", Slot::FileScope);
        session.commit("int value = 21;", Slot::Stmt);
        session.attach_stdin_events(vec![crate::proc::StdinEvent {
            bytes: b"secret input\n".to_vec(),
            eof: false,
        }]);
        let src = build_user_view(&session);
        assert!(src.contains("int twice(int x)"));
        assert!(src.contains("int main(void)"));
        assert!(src.contains("int value = 21;"));
        assert!(src.contains("stdin tape: 1 captured request(s); contents hidden"));
        assert!(!src.contains("secret input"));
        assert!(!src.contains("CS_PRINT"));
        assert!(!src.contains("CS_MARK"));
    }

    #[test]
    fn scoped_statements_and_file_replacements_preserve_program_shape() {
        let mut session = Session::default();
        session.file_items.push("int f(void) { return 1; }".into());
        session
            .file_items
            .push("int g(void) { return f(); }".into());

        let replacement = build_file_replacement(&session, "int f(void) { return 2; }", 0).src;
        let new_f = replacement.find("return 2").expect("replacement present");
        let dependent_g = replacement.find("int g(void)").expect("dependent present");
        assert!(new_f < dependent_g);
        assert!(!replacement.contains("return 1"));

        session.commit_scoped("int x = 2;");
        let future = build(&session, "x", Slot::Expr).src;
        let declaration = future.find("int x = 2;").expect("scoped declaration");
        let query = future.rfind("\nx\n").expect("future query");
        let close = future.rfind("    }\n    return 0;").expect("scope close");
        assert!(declaration < query && query < close);
    }
}
