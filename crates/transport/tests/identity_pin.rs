//! Pruebas de identidad T10b: extracción SPKI con parser DER estándar y pin
//! SHA-256. Los vectores (certificados, SPKI y pines) fueron generados con
//! OpenSSL CLI, no con el código bajo prueba. Son certificados de test; no hay
//! material de producción en el repo.

use arl_transport::identity::{extract_spki_der, IdentityError, SpkiPin, MAX_CERT_DER};

fn hex(s: &str) -> Vec<u8> {
    assert!(s.len() % 2 == 0, "hex de longitud par");
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex válido"))
        .collect()
}

fn pin_hex(pin: SpkiPin) -> String {
    pin.to_hex()
}

const CERT1_HEX: &str = concat!(
    "3082018030820127a0030201020214785b4b1dba024b72ea0488d706c0ea9bda7fa757300a06082a8648ce3d04030230",
    "163114301206035504030c0b61726c2d66697874757265301e170d3236313131343131313730345a170d333631313131",
    "3131313730345a30163114301206035504030c0b61726c2d666978747572653059301306072a8648ce3d020106082a86",
    "48ce3d030107034200040404fdf1c3eacec6af14fbdccb4b78861b1c708fc569b4c6ab0de52b37447bdf0d26ab00a0a5",
    "aeed2352c9286718b5b480ae9e9bddedab074c0a50ebe78ca38da3533051301d0603551d0e04160414b206807d70e59e",
    "16e2219917ad9c77a4d1c76a75301f0603551d23041830168014b206807d70e59e16e2219917ad9c77a4d1c76a75300f",
    "0603551d130101ff040530030101ff300a06082a8648ce3d040302034700304402205aa3d54337e3035a16f6a78e9dcf",
    "c1957fff7108116ac7bb1f4028a7197c7bd302201ac798aafd92a8743ab7e716f7734486eaee1b13a4907ae417a531b2",
    "da0449cf",
);
const CERT2_HEX: &str = concat!(
    "3082018130820127a00302010202141ecfbcc9ff0faaacdc4b33beb6569d0a1ed3ed74300a06082a8648ce3d04030230",
    "163114301206035504030c0b61726c2d66697874757265301e170d3236313131343131313730345a170d333631313131",
    "3131313730345a30163114301206035504030c0b61726c2d666978747572653059301306072a8648ce3d020106082a86",
    "48ce3d03010703420004d91dab384d57b35abea7408e2d4c51a7a38413776065b7acd5ebd4e8d3fa9d22c2652e981ce8",
    "20253602d67884ffeaf63298a84c790b53ceb723e0b6983bc1f3a3533051301d0603551d0e04160414db7607ce734d80",
    "bd341cde7d9c9856b647147800301f0603551d23041830168014db7607ce734d80bd341cde7d9c9856b647147800300f",
    "0603551d130101ff040530030101ff300a06082a8648ce3d0403020348003045022008bb2e4ed099ef1924129b8416ee",
    "6250456c1b7f25e9a26e41a738e0d42d047e022100b57af5a34127f8c5c60b14d730242561175466bcd6d453af0d56df",
    "05bab4a72f",
);
const SPKI1_HEX: &str = concat!(
    "3059301306072a8648ce3d020106082a8648ce3d030107034200040404fdf1c3eacec6af14fbdccb4b78861b1c708fc5",
    "69b4c6ab0de52b37447bdf0d26ab00a0a5aeed2352c9286718b5b480ae9e9bddedab074c0a50ebe78ca38d",
);
const PIN1_HEX: &str = "3be6ff4678596ddb572e8c112f38d35426cc9970e2f36b760f2ecce88fb0fa6a";
const PIN2_HEX: &str = "d07f1d4afef658d72793f4b760ced6aa8bdca2b0f3cf2faacfd57c434105f5a0";

fn cert1() -> Vec<u8> {
    hex(CERT1_HEX)
}

fn cert2() -> Vec<u8> {
    hex(CERT2_HEX)
}

#[test]
fn spki_extraido_coincide_con_spki_conocido() {
    let c = cert1();
    let got = extract_spki_der(&c).expect("certificado válido");
    assert_eq!(got, hex(SPKI1_HEX).as_slice());
}

#[test]
fn pin_coincide_con_sha256_de_spki_openssl() {
    let pin = SpkiPin::from_certificate_der(&cert1()).expect("pin");
    assert_eq!(pin_hex(pin), PIN1_HEX);
}

#[test]
fn mismo_subject_distinta_clave_da_pin_distinto() {
    let c1 = cert1();
    let c2 = cert2();
    assert_ne!(
        extract_spki_der(&c1).unwrap(),
        extract_spki_der(&c2).unwrap()
    );
    let p1 = SpkiPin::from_certificate_der(&c1).unwrap();
    let p2 = SpkiPin::from_certificate_der(&c2).unwrap();
    assert_eq!(pin_hex(p1), PIN1_HEX);
    assert_eq!(pin_hex(p2), PIN2_HEX);
    assert_ne!(p1, p2);
}

#[test]
fn trailing_bytes_tras_certificado_rechazados() {
    let mut c = cert1();
    c.push(0x00);
    assert!(matches!(
        extract_spki_der(&c),
        Err(IdentityError::MalformedDer(_))
    ));
    assert!(SpkiPin::from_certificate_der(&c).is_err());
}

#[test]
fn certificado_truncado_rechazado_en_todos_los_cortes() {
    let full = cert1();
    for cut in 0..full.len() {
        assert!(
            extract_spki_der(&full[..cut]).is_err(),
            "corte en {cut} aceptado"
        );
    }
}

#[test]
fn longitud_indefinida_rechazada() {
    // SEQUENCE exterior con longitud 0x82 0x01 0x80 -> 0x80 (indefinida).
    let mut c = cert1();
    assert_eq!(&c[..4], &[0x30, 0x82, 0x01, 0x80]);
    c[1] = 0x80;
    c.drain(2..4);
    assert!(extract_spki_der(&c).is_err());
}

#[test]
fn longitud_larga_no_canonica_rechazada() {
    // El original declara 384 B con la forma mínima `0x82 0x01 0x80`. La
    // variante no canónica usa `0x83 0x00 0x01 0x80`: mismo valor 384, pero
    // con un byte de longitud de más. El cuerpo queda **idéntico**, así que
    // el único motivo de rechazo posible es la forma no mínima de la longitud;
    // un vector que también cambiara el cuerpo no probaría la canonicidad.
    let original = cert1();
    assert_eq!(&original[..4], &[0x30, 0x82, 0x01, 0x80]);
    let declared = usize::from(original[2]) << 8 | usize::from(original[3]);
    assert_eq!(declared, 384, "longitud declarada del original");
    assert_eq!(declared, original.len() - 4, "longitud declarada = cuerpo");

    let mut mutado = Vec::with_capacity(original.len() + 1);
    mutado.extend_from_slice(&[0x30, 0x83, 0x00, 0x01, 0x80]);
    mutado.extend_from_slice(&original[4..]);
    // Cuerpo byte a byte idéntico al original y longitud consistente con 384.
    assert_eq!(&mutado[5..], &original[4..], "cuerpo idéntico al original");
    assert_eq!(
        mutado.len() - 5,
        declared,
        "cuerpo de 384 B tras la cabecera"
    );

    assert_eq!(
        extract_spki_der(&original).expect("original aceptado"),
        hex(SPKI1_HEX).as_slice()
    );
    assert_eq!(
        extract_spki_der(&mutado),
        Err(IdentityError::MalformedDer("DER estricto rechazado")),
        "la forma larga no mínima debe rechazarla el parser DER"
    );
}

#[test]
fn etiqueta_exterior_no_sequence_rechazada() {
    let mut c = cert1();
    c[0] = 0x31;
    assert!(extract_spki_der(&c).is_err());
}

#[test]
fn serial_con_tipo_incorrecto_rechazado() {
    // Serial INTEGER (0x02) cambiado a OCTET STRING (0x04): el DER estándar
    // lo rechaza; un parser que sólo salta TLV lo aceptaría.
    let mut c = cert1();
    let pos = c.windows(4).position(|w| w == [0x30, 0x23, 0x02, 0x01]);
    assert!(pos.is_none());
    let serial_tag = c
        .windows(2)
        .position(|w| w == [0xa0, 0x03])
        .map(|v| v + 5)
        .expect("serial tras version");
    assert_eq!(c[serial_tag], 0x02);
    c[serial_tag] = 0x04;
    assert!(extract_spki_der(&c).is_err());
}

#[test]
fn certificado_mayor_que_limite_rechazado() {
    let big = vec![0u8; MAX_CERT_DER + 1];
    assert_eq!(
        extract_spki_der(&big),
        Err(IdentityError::CertificateTooLarge)
    );
}

#[test]
fn entrada_vacia_rechazada() {
    assert!(extract_spki_der(&[]).is_err());
}
