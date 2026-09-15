/* config.h.  Generated from config.h.in by configure.  */
/* config.h.in.  Generated from configure.ac by autoheader.  */

#ifndef CONFIG_H
#define CONFIG_H

/* Define if building universal (internal helper macro) */
/* #undef AC_APPLE_UNIVERSAL_BUILD */

/* Set to nonzero if you want to make memcached cluster-aware */
#define ENABLE_CLUSTER_AWARE 1

/* Set to nonzero if you want to include DTRACE */
/* #undef ENABLE_DTRACE */

/* Set to nonzero if you want to include ISASL */
/* #undef ENABLE_ISASL */

/* Set to nonzero if you want to include migration */
/* #undef ENABLE_MIGRATION */

/* Set to nonzero if you want to include persistence */
/* #undef ENABLE_PERSISTENCE */

/* Set to nonzero if you want to include replication */
#define ENABLE_REPLICATION 1

/* Set to nonzero if you want to include SASL */
#define ENABLE_SASL 1

/* Set to nonzero if you want to enable a SASL pwdb */
/* #undef ENABLE_SASL_PWDB */

/* Set to nonzero if you want to include sticky items */
#define ENABLE_STICKY_ITEM 1

/* Set to nonzero if you want to make zookeeper integration */
#define ENABLE_ZK_INTEGRATION 1

/* Set to nonzero if you want to use zookeeper dynamic reconfig */
/* #undef ENABLE_ZK_RECONFIG */

/* Define to 1 if you have the <dlfcn.h> header file. */
#define HAVE_DLFCN_H 1

/* Define this if you have an implementation of drop_privileges() */
/* #undef HAVE_DROP_PRIVILEGES */

/* Define to 1 if you have the 'getpagesizes' function. */
/* #undef HAVE_GETPAGESIZES */

/* Have ntohll */
#define HAVE_HTONLL 1

/* Define to 1 if you have the <inttypes.h> header file. */
#define HAVE_INTTYPES_H 1

/* Define to 1 if you have the <link.h> header file. */
/* #undef HAVE_LINK_H */

/* Define to 1 if you have the 'memcntl' function. */
/* #undef HAVE_MEMCNTL */

/* Define to 1 if you have the 'mlockall' function. */
#define HAVE_MLOCKALL 1

/* Define to 1 if you have the <netdb.h> header file. */
#define HAVE_NETDB_H 1

/* Define to 1 if you have the <netinet/in.h> header file. */
#define HAVE_NETINET_IN_H 1

/* Define to 1 if you have the <netinet/tcp.h> header file. */
#define HAVE_NETINET_TCP_H 1

/* Define to 1 if you have the <priv.h> header file. */
/* #undef HAVE_PRIV_H */

/* Define to 1 if you have the <pwd.h> header file. */
#define HAVE_PWD_H 1

/* Define to 1 if you have the 'setppriv' function. */
/* #undef HAVE_SETPPRIV */

/* Define to 1 if stdbool.h conforms to C99. */
#define HAVE_STDBOOL_H 1

/* Define to 1 if you have the <stdint.h> header file. */
#define HAVE_STDINT_H 1

/* Define to 1 if you have the <stdio.h> header file. */
#define HAVE_STDIO_H 1

/* Define to 1 if you have the <stdlib.h> header file. */
#define HAVE_STDLIB_H 1

/* Define to 1 if you have the <strings.h> header file. */
#define HAVE_STRINGS_H 1

/* Define to 1 if you have the <string.h> header file. */
#define HAVE_STRING_H 1

/* Define to 1 if you have the <sysexits.h> header file. */
#define HAVE_SYSEXITS_H 1

/* Define to 1 if you have the <syslog.h> header file. */
#define HAVE_SYSLOG_H 1

/* Define to 1 if you have the <sys/mman.h> header file. */
#define HAVE_SYS_MMAN_H 1

/* Define to 1 if you have the <sys/resource.h> header file. */
#define HAVE_SYS_RESOURCE_H 1

/* Define to 1 if you have the <sys/socket.h> header file. */
#define HAVE_SYS_SOCKET_H 1

/* Define to 1 if you have the <sys/stat.h> header file. */
#define HAVE_SYS_STAT_H 1

/* Define to 1 if you have the <sys/types.h> header file. */
#define HAVE_SYS_TYPES_H 1

/* Define to 1 if you have the <sys/uio.h> header file. */
#define HAVE_SYS_UIO_H 1

/* Define to 1 if you have the <sys/un.h> header file. */
#define HAVE_SYS_UN_H 1

/* Define to 1 if you have the <sys/wait.h> header file. */
#define HAVE_SYS_WAIT_H 1

/* Define this if you have umem.h */
/* #undef HAVE_UMEM_H */

/* Define to 1 if you have the <unistd.h> header file. */
#define HAVE_UNISTD_H 1

/* Define to 1 if the system has the type '_Bool'. */
#define HAVE__BOOL 1

/* Define to the sub-directory where libtool stores uninstalled libraries. */
#define LT_OBJDIR ".libs/"

/* Machine need alignment */
#define NEED_ALIGN 1

/* Name of package */
#define PACKAGE "arcus-memcached"

/* Define to the address where bug reports for this package should be sent. */
#define PACKAGE_BUGREPORT "openarcus@googlegroups.com"

/* Define to the full name of this package. */
#define PACKAGE_NAME "arcus-memcached"

/* Define to the full name and version of this package. */
#define PACKAGE_STRING "arcus-memcached 0.9.5-E-139"

/* Define to the one symbol short name of this package. */
#define PACKAGE_TARNAME "arcus-memcached"

/* Define to the home page for this package. */
#define PACKAGE_URL ""

/* Define to the version of this package. */
#define PACKAGE_VERSION "0.9.5-E-139"

/* Define to 1 if all of the C89 standard headers exist (not just the ones
   required in a freestanding environment). This macro is provided for
   backward compatibility; new code need not use it. */
#define STDC_HEADERS 1

/* Enable Zookeeper multi thread mode */
#define THREADED 1

/* Defined if clock_gettime is available in libc or librt */
#define USE_CLOCK_GETTIME 1

/* Version number of package */
#define VERSION "0.9.5-E-139"

/* Define WORDS_BIGENDIAN to 1 if your processor stores words with the most
   significant byte first (like Motorola and SPARC, unlike Intel). */
#if defined AC_APPLE_UNIVERSAL_BUILD
# if defined __BIG_ENDIAN__
#  define WORDS_BIGENDIAN 1
# endif
#else
# ifndef WORDS_BIGENDIAN
/* #  undef WORDS_BIGENDIAN */
# endif
#endif

/* make sure IOV_MAX is defined */
#define _GNU_SOURCE 1

/* Define to empty if 'const' does not conform to ANSI C. */
/* #undef const */

/* define to int if socklen_t not available */
/* #undef socklen_t */

#include "config_static.h"
#endif
