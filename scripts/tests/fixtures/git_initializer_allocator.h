/* Fault-injecting libgit2 allocator shared by initializer component tests. */
static int live_allocations, allocation_calls, fail_allocation;

static void *audit_malloc(size_t size, const char *file, int line)
{
    void *result;
    (void)file;
    (void)line;
    if (++allocation_calls == fail_allocation)
        return NULL;
    result = malloc(size);
    if (result)
        live_allocations++;
    return result;
}

static void *audit_realloc(void *ptr, size_t size, const char *file, int line)
{
    void *result;
    int was_null = (ptr == NULL);
    (void)file;
    (void)line;
    if (++allocation_calls == fail_allocation)
        return NULL;
    result = realloc(ptr, size);
    if (result && was_null)
        live_allocations++;
    return result;
}

static void audit_free(void *ptr)
{
    if (ptr)
        live_allocations--;
    free(ptr);
}
