/*
 * Copyright (C) the libgit2 contributors. All rights reserved.
 *
 * This file is part of libgit2, distributed under the GNU GPL v2 with
 * a Linking Exception. For full terms see the included COPYING file.
 */

#include "streams/mbedtls.h"

#ifdef GIT_MBEDTLS

#include <ctype.h>

#include "runtime.h"
#include "errors.h"
#include "stream.h"
#include "streams/socket.h"
#include "git2/transport.h"
#include "util.h"

#ifndef GIT_DEFAULT_CERT_LOCATION
#define GIT_DEFAULT_CERT_LOCATION NULL
#endif

/* Work around C90-conformance issues */
#if !defined(__STDC_VERSION__) || (__STDC_VERSION__ < 199901L)
# if defined(_MSC_VER)
#  define inline __inline
# elif defined(__GNUC__)
#  define inline __inline__
# else
#  define inline
# endif
#endif

#include <mbedtls/ssl.h>
#include <mbedtls/error.h>
#include <mbedtls/entropy.h>
#include <mbedtls/ctr_drbg.h>
#if defined(MBEDTLS_SSL_PROTO_TLS1_3)
#include <psa/crypto.h>
#endif

#undef inline

#define GIT_SSL_DEFAULT_CIPHERS "TLS1-3-AES-128-GCM-SHA256:TLS1-3-AES-256-GCM-SHA384:TLS1-3-CHACHA20-POLY1305-SHA256:TLS-ECDHE-ECDSA-WITH-AES-128-GCM-SHA256:TLS-ECDHE-RSA-WITH-AES-128-GCM-SHA256:TLS-ECDHE-ECDSA-WITH-AES-256-GCM-SHA384:TLS-ECDHE-RSA-WITH-AES-256-GCM-SHA384:TLS-ECDHE-ECDSA-WITH-CHACHA20-POLY1305-SHA256:TLS-ECDHE-RSA-WITH-CHACHA20-POLY1305-SHA256:TLS-DHE-RSA-WITH-AES-128-GCM-SHA256:TLS-DHE-RSA-WITH-AES-256-GCM-SHA384:TLS-DHE-RSA-WITH-CHACHA20-POLY1305-SHA256:TLS-ECDHE-ECDSA-WITH-AES-128-CBC-SHA256:TLS-ECDHE-RSA-WITH-AES-128-CBC-SHA256:TLS-ECDHE-ECDSA-WITH-AES-128-CBC-SHA:TLS-ECDHE-RSA-WITH-AES-128-CBC-SHA:TLS-ECDHE-ECDSA-WITH-AES-256-CBC-SHA384:TLS-ECDHE-RSA-WITH-AES-256-CBC-SHA384:TLS-ECDHE-ECDSA-WITH-AES-256-CBC-SHA:TLS-ECDHE-RSA-WITH-AES-256-CBC-SHA:TLS-DHE-RSA-WITH-AES-128-CBC-SHA256:TLS-DHE-RSA-WITH-AES-256-CBC-SHA256:TLS-RSA-WITH-AES-128-GCM-SHA256:TLS-RSA-WITH-AES-256-GCM-SHA384:TLS-RSA-WITH-AES-128-CBC-SHA256:TLS-RSA-WITH-AES-256-CBC-SHA256:TLS-RSA-WITH-AES-128-CBC-SHA:TLS-RSA-WITH-AES-256-CBC-SHA"
#define GIT_SSL_DEFAULT_CIPHERS_COUNT 28

static int ciphers_list[GIT_SSL_DEFAULT_CIPHERS_COUNT + 1];

static bool initialized = false;
static mbedtls_ctr_drbg_context mbedtls_rng;
static mbedtls_entropy_context mbedtls_entropy;

typedef struct {
	git_atomic32 refs;
	mbedtls_ssl_config config;
	mbedtls_x509_crt ca_chain;
} mbedtls_ca_snapshot;

static git_mutex ca_lock;
static bool ca_lock_initialized = false;
static mbedtls_ca_snapshot *current_ca;

static void ca_snapshot_release(mbedtls_ca_snapshot *snapshot)
{
	if (snapshot && git_atomic32_dec(&snapshot->refs) == 0) {
		mbedtls_ssl_config_free(&snapshot->config);
		mbedtls_x509_crt_free(&snapshot->ca_chain);
		git__free(snapshot);
	}
}

static int ca_snapshot_create(mbedtls_ca_snapshot **out, const char *file, const char *path)
{
	mbedtls_ca_snapshot *snapshot;
	char errbuf[512];
	int ret;

	snapshot = git__calloc(1, sizeof(*snapshot));
	GIT_ERROR_CHECK_ALLOC(snapshot);
	git_atomic32_set(&snapshot->refs, 1);
	mbedtls_ssl_config_init(&snapshot->config);
	mbedtls_x509_crt_init(&snapshot->ca_chain);
	if (mbedtls_ssl_config_defaults(&snapshot->config,
		MBEDTLS_SSL_IS_CLIENT, MBEDTLS_SSL_TRANSPORT_STREAM,
		MBEDTLS_SSL_PRESET_DEFAULT) != 0) {
		git_error_set(GIT_ERROR_SSL, "failed to initialize mbedTLS configuration");
		goto on_error;
	}
#ifdef MBEDTLS_SSL_MINOR_VERSION_2
	mbedtls_ssl_conf_min_version(&snapshot->config, MBEDTLS_SSL_MAJOR_VERSION_3, MBEDTLS_SSL_MINOR_VERSION_2);
#endif
	/* Verification remains in verify_server_cert, including callback handling. */
	mbedtls_ssl_conf_authmode(&snapshot->config, MBEDTLS_SSL_VERIFY_OPTIONAL);
	mbedtls_ssl_conf_ciphersuites(&snapshot->config, ciphers_list);
	mbedtls_ssl_conf_rng(&snapshot->config, mbedtls_ctr_drbg_random, &mbedtls_rng);
	ret = file ? mbedtls_x509_crt_parse_file(&snapshot->ca_chain, file) : 0;
	if (ret == 0 && path)
		ret = mbedtls_x509_crt_parse_path(&snapshot->ca_chain, path);
	/* Positive directory results include skipped parse, I/O and OOM errors.
	 * Publish a complete replacement instead of a partial trust set. */
	if (ret != 0) {
		if (ret > 0)
			git_error_set(GIT_ERROR_SSL, "failed to load %d CA certificate entries", ret);
		else {
			mbedtls_strerror(ret, errbuf, sizeof(errbuf));
			git_error_set(GIT_ERROR_SSL, "failed to load CA certificates: %#04x - %s", (unsigned int)ret, errbuf);
		}
		goto on_error;
	}
	if ((file || path) && (snapshot->ca_chain.version == 0 ||
		!snapshot->ca_chain.raw.p || snapshot->ca_chain.raw.len == 0)) {
		git_error_set(GIT_ERROR_SSL, "no CA certificates could be loaded");
		goto on_error;
	}
	if (file || path)
		mbedtls_ssl_conf_ca_chain(&snapshot->config, &snapshot->ca_chain, NULL);
	*out = snapshot;
	return 0;

on_error:
	ca_snapshot_release(snapshot);
	return -1;
}

static int ca_snapshot_clone(mbedtls_ca_snapshot **out, const mbedtls_ca_snapshot *source)
{
	mbedtls_ca_snapshot *copy;
	const mbedtls_x509_crt *certificate;
	mbedtls_x509_crt *tail;

	if (ca_snapshot_create(&copy, NULL, NULL) < 0)
		return -1;
	tail = &copy->ca_chain;
	/* X.509 verification may mutate public-key contexts. Share only the
	 * immutable encoded certificates, never a live key/config between streams. */
	for (certificate = &source->ca_chain; certificate && certificate->version;
		certificate = certificate->next) {
		if (mbedtls_x509_crt_parse_der(tail,
			certificate->raw.p, certificate->raw.len) != 0) {
			git_error_set(GIT_ERROR_SSL, "failed to copy CA certificates");
			ca_snapshot_release(copy);
			return -1;
		}
		/* Append from the tail instead of rescanning the entire store. */
		while (tail->next)
			tail = tail->next;
	}
	if (copy->ca_chain.raw.len)
		mbedtls_ssl_conf_ca_chain(&copy->config, &copy->ca_chain, NULL);
	*out = copy;
	return 0;
}

static int ca_snapshot_acquire(mbedtls_ca_snapshot **out)
{
	mbedtls_ca_snapshot *snapshot;
	int error;

	if (!ca_lock_initialized || git_mutex_lock(&ca_lock) != 0) {
		git_error_set(GIT_ERROR_SSL, "failed to lock mbedTLS configuration");
		return -1;
	}
	snapshot = current_ca;
	if (snapshot)
		git_atomic32_inc(&snapshot->refs);
	git_mutex_unlock(&ca_lock);
	if (!snapshot) {
		git_error_set(GIT_ERROR_SSL, "mbedTLS configuration is unavailable");
		return -1;
	}
	/* Parsing and setup stay outside the publication lock. Each stream owns
	 * independent verification state; source bytes stay alive until copied. */
	error = ca_snapshot_clone(out, snapshot);
	ca_snapshot_release(snapshot);
	return error;
}

/**
 * This function aims to clean-up the SSL context which
 * we allocated.
 */
static void shutdown_ssl(void)
{
	/* libgit2's runtime must outlive its active streams and API calls. */
	ca_snapshot_release(current_ca);
	current_ca = NULL;
	if (initialized) {
		mbedtls_ctr_drbg_free(&mbedtls_rng);
		mbedtls_entropy_free(&mbedtls_entropy);
		initialized = false;
	}
	if (ca_lock_initialized) {
		git_mutex_free(&ca_lock);
		ca_lock_initialized = false;
	}
}

int git_mbedtls_stream_global_init(void)
{
	int loaded = 0;
	char *crtpath = GIT_DEFAULT_CERT_LOCATION;
	struct stat statbuf;

	size_t ciphers_known = 0;
	char *cipher_name = NULL;
	char *cipher_string = NULL;
	char *cipher_string_tmp = NULL;

	if (git_mutex_init(&ca_lock) != 0) {
		git_error_set(GIT_ERROR_SSL, "failed to initialize mbedTLS configuration lock");
		return -1;
	}
	ca_lock_initialized = true;
	mbedtls_entropy_init(&mbedtls_entropy);
	mbedtls_ctr_drbg_init(&mbedtls_rng);
	initialized = true;

#if defined(MBEDTLS_SSL_PROTO_TLS1_3)
	/* Complete PSA initialization before a stream can start a TLS handshake. */
	{
		psa_status_t status = psa_crypto_init();
		if (status != PSA_SUCCESS) {
			git_error_set(GIT_ERROR_SSL, "failed to initialize PSA crypto: %d", (int)status);
			goto cleanup;
		}
	}
#endif

	/* set the list of allowed ciphersuites */
	ciphers_known = 0;
	cipher_string = cipher_string_tmp = git__strdup(GIT_SSL_DEFAULT_CIPHERS);
	if (!cipher_string)
		goto cleanup;

	while ((cipher_name = git__strtok(&cipher_string_tmp, ":")) != NULL) {
		int cipherid = mbedtls_ssl_get_ciphersuite_id(cipher_name);
		if (cipherid == 0) continue;

		if (ciphers_known >= ARRAY_SIZE(ciphers_list) - 1) {
			git_error_set(GIT_ERROR_SSL, "out of cipher list space");
			goto cleanup;
		}

		ciphers_list[ciphers_known++] = cipherid;
	}
	git__free(cipher_string);
	cipher_string = NULL;
	ciphers_list[ciphers_known] = 0;

	if (!ciphers_known) {
		git_error_set(GIT_ERROR_SSL, "no cipher could be enabled");
		goto cleanup;
	}

	/* Seeding the random number generator */

	if (mbedtls_ctr_drbg_seed(&mbedtls_rng, mbedtls_entropy_func,
			&mbedtls_entropy, NULL, 0) != 0) {
		git_error_set(GIT_ERROR_SSL, "failed to initialize mbedTLS entropy pool");
		goto cleanup;
	}

	/* Prime configuration defaults before concurrent callers can build more. */
	if (ca_snapshot_create(&current_ca, NULL, NULL) < 0)
		goto cleanup;

	/* load default certificates */
	if (crtpath != NULL && stat(crtpath, &statbuf) == 0 && S_ISREG(statbuf.st_mode))
		loaded = (git_mbedtls__set_cert_location(crtpath, NULL) == 0);

	if (!loaded && crtpath != NULL && stat(crtpath, &statbuf) == 0 && S_ISDIR(statbuf.st_mode))
		loaded = (git_mbedtls__set_cert_location(NULL, crtpath) == 0);

	if (git_runtime_shutdown_register(shutdown_ssl) < 0)
		goto cleanup;
	return 0;

cleanup:
	git__free(cipher_string);
	shutdown_ssl();

	return -1;
}

static int bio_read(void *b, unsigned char *buf, size_t len)
{
	git_stream *io = (git_stream *) b;
	return (int) git_stream_read(io, buf, min(len, INT_MAX));
}

static int bio_write(void *b, const unsigned char *buf, size_t len)
{
	git_stream *io = (git_stream *) b;
	return (int) git_stream_write(io, (const char *)buf, min(len, INT_MAX), 0);
}

static int ssl_set_error(mbedtls_ssl_context *ssl, int error)
{
	char errbuf[512];
	int ret = -1;

	GIT_ASSERT(error != MBEDTLS_ERR_SSL_WANT_READ);
	GIT_ASSERT(error != MBEDTLS_ERR_SSL_WANT_WRITE);

	if (error != 0)
		mbedtls_strerror( error, errbuf, 512 );

	switch(error) {
		case 0:
		git_error_set(GIT_ERROR_SSL, "SSL error: unknown error");
		break;

	case MBEDTLS_ERR_X509_CERT_VERIFY_FAILED:
		git_error_set(GIT_ERROR_SSL, "SSL error: %#04x [%x] - %s", error, mbedtls_ssl_get_verify_result(ssl), errbuf);
		ret = GIT_ECERTIFICATE;
		break;

	default:
		git_error_set(GIT_ERROR_SSL, "SSL error: %#04x - %s", error, errbuf);
	}

	return ret;
}

static int ssl_teardown(mbedtls_ssl_context *ssl)
{
	int ret = 0;

	ret = mbedtls_ssl_close_notify(ssl);
	if (ret < 0)
		ret = ssl_set_error(ssl, ret);

	mbedtls_ssl_free(ssl);
	return ret;
}

static int verify_server_cert(mbedtls_ssl_context *ssl)
{
	int ret = -1;

	if ((ret = mbedtls_ssl_get_verify_result(ssl)) != 0) {
		char vrfy_buf[512];
		int len = mbedtls_x509_crt_verify_info(vrfy_buf, sizeof(vrfy_buf), "", ret);
		if (len >= 1) vrfy_buf[len - 1] = '\0'; /* Remove trailing \n */
		git_error_set(GIT_ERROR_SSL, "the SSL certificate is invalid: %#04x - %s", ret, vrfy_buf);
		return GIT_ECERTIFICATE;
	}

	return 0;
}

typedef struct {
	git_stream parent;
	git_stream *io;
	int owned;
	bool connected;
	char *host;
	mbedtls_ssl_context *ssl;
	mbedtls_ca_snapshot *snapshot;
	git_cert_x509 cert_info;
} mbedtls_stream;


static int mbedtls_connect(git_stream *stream)
{
	int ret;
	mbedtls_stream *st = (mbedtls_stream *) stream;

	if (st->owned && (ret = git_stream_connect(st->io)) < 0)
		return ret;

	st->connected = true;

	if ((ret = mbedtls_ssl_set_hostname(st->ssl, st->host)) != 0)
		return ssl_set_error(st->ssl, ret);

	mbedtls_ssl_set_bio(st->ssl, st->io, bio_write, bio_read, NULL);

	if ((ret = mbedtls_ssl_handshake(st->ssl)) != 0)
		return ssl_set_error(st->ssl, ret);

	return verify_server_cert(st->ssl);
}

static int mbedtls_certificate(git_cert **out, git_stream *stream)
{
	unsigned char *encoded_cert;
	mbedtls_stream *st = (mbedtls_stream *) stream;

	const mbedtls_x509_crt *cert = mbedtls_ssl_get_peer_cert(st->ssl);
	if (!cert) {
		git_error_set(GIT_ERROR_SSL, "the server did not provide a certificate");
		return -1;
	}

	/* Retrieve the length of the certificate first */
	if (cert->raw.len == 0) {
		git_error_set(GIT_ERROR_NET, "failed to retrieve certificate information");
		return -1;
	}

	encoded_cert = git__malloc(cert->raw.len);
	GIT_ERROR_CHECK_ALLOC(encoded_cert);
	memcpy(encoded_cert, cert->raw.p, cert->raw.len);

	st->cert_info.parent.cert_type = GIT_CERT_X509;
	st->cert_info.data = encoded_cert;
	st->cert_info.len = cert->raw.len;

	*out = &st->cert_info.parent;

	return 0;
}

static int mbedtls_set_proxy(git_stream *stream, const git_proxy_options *proxy_options)
{
	mbedtls_stream *st = (mbedtls_stream *) stream;

	return git_stream_set_proxy(st->io, proxy_options);
}

static ssize_t mbedtls_stream_write(git_stream *stream, const char *data, size_t len, int flags)
{
	mbedtls_stream *st = (mbedtls_stream *) stream;
	int written;

	GIT_UNUSED(flags);

	/*
	 * `mbedtls_ssl_write` can only represent INT_MAX bytes
	 * written via its return value. We thus need to clamp
	 * the maximum number of bytes written.
	 */
	len = min(len, INT_MAX);

	if ((written = mbedtls_ssl_write(st->ssl, (const unsigned char *)data, len)) <= 0)
		return ssl_set_error(st->ssl, written);

	return written;
}

static ssize_t mbedtls_stream_read(git_stream *stream, void *data, size_t len)
{
	mbedtls_stream *st = (mbedtls_stream *) stream;
	int ret;

	if ((ret = mbedtls_ssl_read(st->ssl, (unsigned char *)data, len)) <= 0)
		ssl_set_error(st->ssl, ret);

	return ret;
}

static int mbedtls_stream_close(git_stream *stream)
{
	mbedtls_stream *st = (mbedtls_stream *) stream;
	git_error *last_error = NULL;
	int ret = 0;

	if (st->connected && ssl_teardown(st->ssl) != 0)
		ret = -1;

	st->connected = false;

	if (st->owned) {
		int close_ret;
		if (ret)
			git_error_save(&last_error);
		close_ret = git_stream_close(st->io);
		if (last_error)
			git_error_restore(last_error);
		if (!ret)
			ret = close_ret;
	}

	return ret;
}

static void mbedtls_stream_free(git_stream *stream)
{
	mbedtls_stream *st = (mbedtls_stream *) stream;

	if (st->owned)
		git_stream_free(st->io);

	git__free(st->host);
	git__free(st->cert_info.data);
	mbedtls_ssl_free(st->ssl);
	git__free(st->ssl);
	ca_snapshot_release(st->snapshot);
	git__free(st);
}

static int mbedtls_stream_wrap(
	git_stream **out,
	git_stream *in,
	const char *host,
	int owned)
{
	mbedtls_stream *st;
	int error = -1;

	st = git__calloc(1, sizeof(mbedtls_stream));
	GIT_ERROR_CHECK_ALLOC(st);

	st->io = in;
	st->owned = owned;

	st->ssl = git__malloc(sizeof(mbedtls_ssl_context));
	if (!st->ssl)
		goto out_err;
	mbedtls_ssl_init(st->ssl);
	if (ca_snapshot_acquire(&st->snapshot) < 0)
		goto out_err;
	if (mbedtls_ssl_setup(st->ssl, &st->snapshot->config)) {
		git_error_set(GIT_ERROR_SSL, "failed to create ssl object");
		error = -1;
		goto out_err;
	}

	st->host = git__strdup(host);
	if (!st->host)
		goto out_err;

	st->parent.version = GIT_STREAM_VERSION;
	st->parent.encrypted = 1;
	st->parent.proxy_support = git_stream_supports_proxy(st->io);
	st->parent.connect = mbedtls_connect;
	st->parent.certificate = mbedtls_certificate;
	st->parent.set_proxy = mbedtls_set_proxy;
	st->parent.read = mbedtls_stream_read;
	st->parent.write = mbedtls_stream_write;
	st->parent.close = mbedtls_stream_close;
	st->parent.free = mbedtls_stream_free;

	*out = (git_stream *) st;
	return 0;

out_err:
	/* The input stream remains the caller's responsibility until success. */
	if (st->ssl)
		mbedtls_ssl_free(st->ssl);
	git__free(st->ssl);
	ca_snapshot_release(st->snapshot);
	git__free(st->host);
	git__free(st);

	return error;
}

int git_mbedtls_stream_wrap(
	git_stream **out,
	git_stream *in,
	const char *host)
{
	return mbedtls_stream_wrap(out, in, host, 0);
}

int git_mbedtls_stream_new(
	git_stream **out,
	const char *host,
	const char *port)
{
	git_stream *stream;
	git_error *last_error = NULL;
	int error;

	GIT_ASSERT_ARG(out);
	GIT_ASSERT_ARG(host);
	GIT_ASSERT_ARG(port);

	if ((error = git_socket_stream_new(&stream, host, port)) < 0)
		return error;

	if ((error = mbedtls_stream_wrap(out, stream, host, 1)) < 0) {
		if (git_error_exists())
			git_error_save(&last_error);
		git_stream_close(stream);
		git_stream_free(stream);
		if (last_error)
			git_error_restore(last_error);
		else
			git_error_clear();
	}

	return error;
}

int git_mbedtls__set_cert_location(const char *file, const char *path)
{
	mbedtls_ca_snapshot *candidate, *previous;

	GIT_ASSERT_ARG(file || path);
	if (!ca_lock_initialized) {
		git_error_set(GIT_ERROR_SSL, "mbedTLS configuration is unavailable");
		return -1;
	}
	/* Build a replacement without changing configuration in active streams. */
	if (ca_snapshot_create(&candidate, file, path) < 0)
		return -1;
	if (git_mutex_lock(&ca_lock) != 0) {
		ca_snapshot_release(candidate);
		git_error_set(GIT_ERROR_SSL, "failed to lock mbedTLS configuration");
		return -1;
	}
	previous = current_ca;
	current_ca = candidate;
	git_mutex_unlock(&ca_lock);
	ca_snapshot_release(previous);
	return 0;
}

#else

#include "stream.h"

int git_mbedtls_stream_global_init(void)
{
	return 0;
}

#endif
