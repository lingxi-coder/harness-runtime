/* Local TLS peers with distinct trust snapshots; Git adapters are test substitutes. */
enum { PAIRS = 6, THREADS = 2 * PAIRS };
static mbedtls_entropy_context entropy;
static mbedtls_ctr_drbg_context rng;
static atomic_bool stop_updates;
static unsigned updates_a, updates_b;
static pthread_mutex_t start_mutex = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t start_condition = PTHREAD_COND_INITIALIZER;
static int waiting;
static pthread_mutex_t update_mutex = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t update_condition = PTHREAD_COND_INITIALIZER;
static int client_sends_waiting;
static bool first_updates_complete;

struct peer {
    int fd, server, authority, trust, hostname_ok, version, result;
    unsigned flags;
    mbedtls_ca_snapshot *snapshot;
};

static const char *cert_path(int authority) { return authority ? TEST_CERT_B : TEST_CERT_A; }
static const char *key_path(int authority) { return authority ? TEST_KEY_B : TEST_KEY_A; }

static int send_cb(void *ctx, const unsigned char *buf, size_t n) {
    struct peer *p = ctx;
    if (!p->server) {
        /* Hold a real handshake in its send callback during two CA replacements. */
        pthread_mutex_lock(&update_mutex);
        ++client_sends_waiting;
        pthread_cond_broadcast(&update_condition);
        while (!first_updates_complete) pthread_cond_wait(&update_condition, &update_mutex);
        pthread_mutex_unlock(&update_mutex);
    }
    ssize_t result = send(p->fd, buf, n, 0);
    return result < 0 ? MBEDTLS_ERR_SSL_INTERNAL_ERROR : (int)result;
}
static int recv_cb(void *ctx, unsigned char *buf, size_t n) {
    struct peer *p = ctx;
    ssize_t result = recv(p->fd, buf, n, 0);
    return result < 0 ? MBEDTLS_ERR_SSL_INTERNAL_ERROR : (int)result;
}
static void rendezvous(void) {
    pthread_mutex_lock(&start_mutex);
    if (++waiting == THREADS + 1) pthread_cond_broadcast(&start_condition);
    while (waiting < THREADS + 1) pthread_cond_wait(&start_condition, &start_mutex);
    pthread_mutex_unlock(&start_mutex);
}
static void *update_ca(void *unused) {
    (void)unused;
    rendezvous();
    pthread_mutex_lock(&update_mutex);
    while (!client_sends_waiting) pthread_cond_wait(&update_condition, &update_mutex);
    pthread_mutex_unlock(&update_mutex);
    assert(git_mbedtls__set_cert_location(TEST_CERT_B, NULL) == 0); ++updates_b;
    assert(git_mbedtls__set_cert_location(TEST_CERT_A, NULL) == 0); ++updates_a;
    pthread_mutex_lock(&update_mutex);
    first_updates_complete = true;
    pthread_cond_broadcast(&update_condition);
    pthread_mutex_unlock(&update_mutex);
    while (!atomic_load(&stop_updates)) {
        assert(git_mbedtls__set_cert_location(TEST_CERT_B, NULL) == 0); ++updates_b;
        assert(git_mbedtls__set_cert_location(TEST_CERT_A, NULL) == 0); ++updates_a;
    }
    return NULL;
}
static void *run_peer(void *argument) {
    struct peer *p = argument;
    mbedtls_ssl_context ssl;
    mbedtls_ssl_config peer_config;
    mbedtls_x509_crt peer_certificate;
    mbedtls_pk_context peer_key;
    int r = 0;
    mbedtls_ssl_config_init(&peer_config);
    mbedtls_x509_crt_init(&peer_certificate);
    mbedtls_pk_init(&peer_key);
    mbedtls_ssl_init(&ssl);
    if (p->server) {
        r = mbedtls_x509_crt_parse_file(&peer_certificate, cert_path(p->authority));
        if (!r) r = mbedtls_pk_parse_keyfile(&peer_key, key_path(p->authority), NULL, mbedtls_ctr_drbg_random, &rng);
        if (!r) r = mbedtls_ssl_config_defaults(&peer_config, MBEDTLS_SSL_IS_SERVER, MBEDTLS_SSL_TRANSPORT_STREAM, MBEDTLS_SSL_PRESET_DEFAULT);
        if (!r) {
            mbedtls_ssl_conf_rng(&peer_config, mbedtls_ctr_drbg_random, &rng);
            mbedtls_ssl_conf_min_tls_version(&peer_config, p->version);
            mbedtls_ssl_conf_max_tls_version(&peer_config, p->version);
            r = mbedtls_ssl_conf_own_cert(&peer_config, &peer_certificate, &peer_key);
        }
    }
    if (!r) r = mbedtls_ssl_setup(&ssl, p->server ? &peer_config : &p->snapshot->config);
    if (!r && !p->server) r = mbedtls_ssl_set_hostname(&ssl, p->hostname_ok ? "expected.test" : "wrong.test");
    if (!r) mbedtls_ssl_set_bio(&ssl, p, send_cb, recv_cb, NULL);
    rendezvous();
    if (!r) r = mbedtls_ssl_handshake(&ssl);
    if (!r && !p->server) r = verify_server_cert(&ssl);
    p->flags = mbedtls_ssl_get_verify_result(&ssl);
    p->result = r;
    if (r != 0) shutdown(p->fd, SHUT_RDWR);
    mbedtls_ssl_free(&ssl);
    ca_snapshot_release(p->snapshot);
    mbedtls_ssl_config_free(&peer_config);
    mbedtls_pk_free(&peer_key);
    mbedtls_x509_crt_free(&peer_certificate);
    return NULL;
}
static int run_version(int version) {
    /* Both authorities, both cross-trust failures, and both hostname failures. */
    const int authority[PAIRS] = {0, 1, 1, 0, 0, 1};
    const int trust[PAIRS] =     {0, 0, 1, 1, 0, 1};
    const int hostname_ok[PAIRS] = {1, 1, 1, 1, 0, 0};
    struct peer peers[THREADS] = {0};
    pthread_t threads[THREADS];
    waiting = 0; client_sends_waiting = 0; first_updates_complete = false;
    updates_a = updates_b = 0; atomic_store(&stop_updates, false);
    for (int i = 0; i < PAIRS; i++) {
        int sockets[2];
        assert(git_mbedtls__set_cert_location(cert_path(trust[i]), NULL) == 0);
        mbedtls_ca_snapshot *snapshot = NULL;
        assert(ca_snapshot_acquire(&snapshot) == 0);
        if (socketpair(AF_UNIX, SOCK_STREAM, 0, sockets) != 0) return 1;
        struct timeval timeout = {10, 0};
        setsockopt(sockets[0], SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout));
        setsockopt(sockets[1], SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout));
        peers[2*i] = (struct peer){.fd=sockets[0], .server=1, .authority=authority[i], .version=version};
        peers[2*i+1] = (struct peer){.fd=sockets[1], .authority=authority[i], .trust=trust[i],
            .hostname_ok=hostname_ok[i], .version=version, .snapshot=snapshot};
    }
    pthread_t updater;
    if (pthread_create(&updater, NULL, update_ca, NULL)) return 2;
    for (int i=0; i<THREADS; i++) if (pthread_create(&threads[i], NULL, run_peer, &peers[i])) return 2;
    for (int i=0; i<THREADS; i++) pthread_join(threads[i], NULL);
    atomic_store(&stop_updates, true); pthread_join(updater, NULL);
    assert(updates_a > 0 && updates_b > 0 && first_updates_complete);
    printf("CA updates: A=%u B=%u; two replacements while a handshake waited in send\n", updates_a, updates_b);
    int failures = 0;
    for (int i=0; i<PAIRS; i++) {
        struct peer *server = &peers[2*i], *client = &peers[2*i+1];
        bool expected_success = client->trust == client->authority && client->hostname_ok;
        bool ok = expected_success ? (client->result == 0 && server->result == 0 && client->flags == 0) :
            (client->result != 0 && (client->flags & (client->hostname_ok ?
                MBEDTLS_X509_BADCERT_NOT_TRUSTED : MBEDTLS_X509_BADCERT_CN_MISMATCH)) != 0);
        printf("TLS 0x%x pair=%d authority=%c trust=%c hostname=%s client=%d flags=0x%x server=%d %s\n",
               version, i, 'A'+client->authority, 'A'+client->trust,
               client->hostname_ok ? "expected.test" : "wrong.test", client->result,
               client->flags, server->result, ok ? "PASS" : "FAIL");
        failures += !ok;
        close(server->fd); close(client->fd);
    }
    return failures != 0;
}
int main(void) {
    signal(SIGPIPE, SIG_IGN);
    mbedtls_entropy_init(&entropy); mbedtls_ctr_drbg_init(&rng);
    if (git_mbedtls_stream_global_init() != 0) return 2;
    int r = mbedtls_ctr_drbg_seed(&rng, mbedtls_entropy_func, &entropy,
                                  (const unsigned char *)"concurrent-probe", 16);
    if (!r) r = run_version(MBEDTLS_SSL_VERSION_TLS1_3);
    if (!r) r = run_version(MBEDTLS_SSL_VERSION_TLS1_2);
    shutdown_ssl();
    assert(atomic_load(&owned_allocations) == 0);
    mbedtls_ctr_drbg_free(&rng); mbedtls_entropy_free(&entropy);
    return r ? 1 : 0;
}
