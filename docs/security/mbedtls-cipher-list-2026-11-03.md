# Mbed TLS cipher-list termination

The Mbed TLS cipher-suite configuration consumes a zero-terminated list. The
list now reserves a terminator in addition to every named suite and writes it
on each initialization, including when a later initialization recognizes fewer
suites. Without the extra slot, a configuration that recognized every named
suite could read beyond the list; this does not establish that the current
device configuration recognizes every suite.

The source-extraction test makes all listed names recognizable, then consumes
the array through its terminator. It also checks a later shorter list. The old
array triggers an AddressSanitizer global-buffer-overflow in the all-recognized
test. The test uses callbacks and does not perform network or CA operations.
