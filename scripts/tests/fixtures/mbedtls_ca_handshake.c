/* Local socket TLS peers; product CA/verify bodies are supplied by the Python test. */
enum { PAIRS = 4, THREADS = 2 * PAIRS };
static mbedtls_entropy_context entropy;
static mbedtls_ctr_drbg_context rng;
static mbedtls_x509_crt certificate;
static mbedtls_pk_context key;
static atomic_bool stop_updates;
static unsigned update_count;
static mbedtls_ssl_config server_config;
static pthread_mutex_t start_mutex = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t start_condition = PTHREAD_COND_INITIALIZER;
static int waiting;

struct peer { int fd, server, good, version, result; unsigned flags; };
static int send_cb(void *ctx, const unsigned char *buf, size_t n) {
    ssize_t result = send(*(int *)ctx, buf, n, 0);
    return result < 0 ? MBEDTLS_ERR_SSL_INTERNAL_ERROR : (int)result;
}
static int recv_cb(void *ctx, unsigned char *buf, size_t n) {
    ssize_t result = recv(*(int *)ctx, buf, n, 0);
    return result < 0 ? MBEDTLS_ERR_SSL_INTERNAL_ERROR : (int)result;
}
static void rendezvous(void) {
    pthread_mutex_lock(&start_mutex);
    if (++waiting == THREADS + 1) pthread_cond_broadcast(&start_condition);
    while (waiting < THREADS + 1) pthread_cond_wait(&start_condition, &start_mutex);
    pthread_mutex_unlock(&start_mutex);
}
static void *update_ca(void *unused) {
 (void)unused; rendezvous();
 do { assert(git_mbedtls__set_cert_location(TEST_CERT, NULL) == 0); ++update_count; }
 while (!atomic_load(&stop_updates)); return NULL;
}
static void *run_peer(void *argument) {
    struct peer *p = argument;
    mbedtls_ssl_context ssl;
    int r = 0;
    mbedtls_ca_snapshot *snapshot = NULL;
    mbedtls_ssl_config peer_config;
    mbedtls_x509_crt peer_certificate;
    mbedtls_pk_context peer_key;
    mbedtls_ssl_config_init(&peer_config);
    mbedtls_x509_crt_init(&peer_certificate);
    mbedtls_pk_init(&peer_key);
    mbedtls_ssl_init(&ssl);
    if (!p->server) r = ca_snapshot_acquire(&snapshot);
    if (p->server) {
        r = mbedtls_x509_crt_parse_file(&peer_certificate, TEST_CERT);
        if (!r) r = mbedtls_pk_parse_keyfile(&peer_key, TEST_KEY, NULL, mbedtls_ctr_drbg_random, &rng);
        if (!r) r = mbedtls_ssl_config_defaults(&peer_config, MBEDTLS_SSL_IS_SERVER, MBEDTLS_SSL_TRANSPORT_STREAM, MBEDTLS_SSL_PRESET_DEFAULT);
        if (!r) {
            mbedtls_ssl_conf_rng(&peer_config, mbedtls_ctr_drbg_random, &rng);
            mbedtls_ssl_conf_min_tls_version(&peer_config, p->version);
            mbedtls_ssl_conf_max_tls_version(&peer_config, p->version);
            r = mbedtls_ssl_conf_own_cert(&peer_config, &peer_certificate, &peer_key);
        }
    }
    if (!r) r = mbedtls_ssl_setup(&ssl, p->server ? &peer_config : &snapshot->config);
    if (r == 0 && !p->server) r = mbedtls_ssl_set_hostname(&ssl, p->good ? "expected.test" : "wrong.test");
    if (r == 0) mbedtls_ssl_set_bio(&ssl, &p->fd, send_cb, recv_cb, NULL);
    rendezvous();
    if (r == 0) r = mbedtls_ssl_handshake(&ssl);
    if (!r && !p->server) r = verify_server_cert(&ssl);
    p->flags = mbedtls_ssl_get_verify_result(&ssl);
    p->result = r;
    if (r != 0) shutdown(p->fd, SHUT_RDWR);
    mbedtls_ssl_free(&ssl);
    ca_snapshot_release(snapshot);
    mbedtls_ssl_config_free(&peer_config);
    mbedtls_pk_free(&peer_key);
    mbedtls_x509_crt_free(&peer_certificate);
    return NULL;
}
static int run_version(int version) {
    struct peer peers[THREADS];
    pthread_t threads[THREADS];
    mbedtls_ssl_conf_min_tls_version(&server_config, version);
    mbedtls_ssl_conf_max_tls_version(&server_config, version);
    waiting = 0;
    for (int i=0; i<PAIRS; i++) {
        int sockets[2];
        if (socketpair(AF_UNIX, SOCK_STREAM, 0, sockets) != 0) return 1;
        struct timeval timeout = {10, 0};
        setsockopt(sockets[0], SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout));
        setsockopt(sockets[1], SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout));
        peers[2*i] = (struct peer){.fd=sockets[0], .server=1, .good=(i%2)==0, .version=version};
        peers[2*i+1] = (struct peer){.fd=sockets[1], .server=0, .good=(i%2)==0, .version=version};
    }
    atomic_store(&stop_updates, false); update_count = 0;
    pthread_t updater;
    if (pthread_create(&updater, NULL, update_ca, NULL)) return 2;
    for (int i=0; i<THREADS; i++) if (pthread_create(&threads[i], NULL, run_peer, &peers[i])) return 2;
    for (int i=0; i<THREADS; i++) pthread_join(threads[i], NULL);
    atomic_store(&stop_updates, true); pthread_join(updater, NULL);
    assert(update_count > 0); printf("CA updates=%u\n", update_count);
    int failures = 0;
    for (int i=0; i<PAIRS; i++) {
        struct peer *server = &peers[2*i], *client = &peers[2*i+1];
        int ok = client->good ? (client->result == 0 && server->result == 0 && client->flags == 0) :
                 (client->result != 0 && (client->flags & MBEDTLS_X509_BADCERT_CN_MISMATCH) != 0);
        printf("TLS 0x%x pair=%d hostname=%s client=%d flags=0x%x server=%d %s\n", version, i,
               client->good ? "expected.test" : "wrong.test", client->result, client->flags,
               server->result, ok ? "PASS" : "FAIL");
        failures += !ok;
        close(server->fd);
        close(client->fd);
    }
    return failures != 0;
}
int main(void) {
    signal(SIGPIPE, SIG_IGN);
    mbedtls_entropy_init(&entropy);
    mbedtls_ctr_drbg_init(&rng);
    mbedtls_x509_crt_init(&certificate);
    mbedtls_pk_init(&key);
    if (git_mbedtls_stream_global_init() != 0) return 2;
    mbedtls_ssl_config_init(&server_config);
    int r = mbedtls_ctr_drbg_seed(&rng, mbedtls_entropy_func, &entropy, (const unsigned char *)"concurrent-probe", 16);
    if (!r) r = mbedtls_x509_crt_parse_file(&certificate, TEST_CERT);
    if (!r) r = mbedtls_pk_parse_keyfile(&key, TEST_KEY, NULL, mbedtls_ctr_drbg_random, &rng);
    /* Match libgit2's serial Once: initialize shared configs before workers. */
    if (!r) r = git_mbedtls__set_cert_location(TEST_CERT, NULL);
    if (!r) r = mbedtls_ssl_config_defaults(&server_config, MBEDTLS_SSL_IS_SERVER, MBEDTLS_SSL_TRANSPORT_STREAM, MBEDTLS_SSL_PRESET_DEFAULT);
    if (!r) {
        mbedtls_ssl_conf_rng(&server_config, mbedtls_ctr_drbg_random, &rng);
        r = mbedtls_ssl_conf_own_cert(&server_config, &certificate, &key);
    }
    if (!r) r = run_version(MBEDTLS_SSL_VERSION_TLS1_3); /* no prior psa_crypto_init() */
    if (!r) r = run_version(MBEDTLS_SSL_VERSION_TLS1_2);
    mbedtls_ssl_config_free(&server_config);
    shutdown_ssl();
    assert(atomic_load(&owned_allocations) == 0);
    mbedtls_pk_free(&key);
    mbedtls_x509_crt_free(&certificate);
    mbedtls_ctr_drbg_free(&rng);
    mbedtls_entropy_free(&entropy);
    return r ? 1 : 0;
}
