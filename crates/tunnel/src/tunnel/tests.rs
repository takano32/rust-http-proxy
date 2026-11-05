use super::open::Reported;
use super::*;

#[test]
fn reported_keeps_the_kind_and_the_message() {
    // 呼び元は種類で EOF 系を見分け、文面は debug に出す (T.28.4)。包んでも変わらないこと
    let inner = io::Error::new(io::ErrorKind::ConnectionRefused, "refused");
    let e = io::Error::new(inner.kind(), Reported(inner));
    assert!(is_reported(&e));
    assert_eq!(e.kind(), io::ErrorKind::ConnectionRefused);
    assert_eq!(e.to_string(), "refused");
    assert!(!is_reported(&io::Error::other("plain")));
    assert!(!is_reported(&io::Error::from(io::ErrorKind::TimedOut)));
}
