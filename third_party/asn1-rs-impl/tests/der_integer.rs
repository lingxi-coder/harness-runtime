use asn1_rs_impl::encode_int;

#[test]
fn positive_der_integer_boundaries() {
    assert_eq!(&encode_int!(0), &[0x00]);
    assert_eq!(&encode_int!(127), &[0x7f]);
    assert_eq!(&encode_int!(128), &[0x00, 0x80]);
    assert_eq!(&encode_int!(255), &[0x00, 0xff]);
    assert_eq!(&encode_int!(256), &[0x01, 0x00]);
    assert_eq!(&encode_int!(32767), &[0x7f, 0xff]);
    assert_eq!(&encode_int!(32768), &[0x00, 0x80, 0x00]);
    assert_eq!(
        &encode_int!(18446744073709551615),
        &[0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]
    );
}
