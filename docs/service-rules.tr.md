# Servis kuralları

- İstek consumer kontratından tam RNG ücretiyle açılır. Consumer, client seed, mapping, request block ve sıfır olmayan `_refundAddress` sabitlenir.
- Keeper her 200 blokluk epoch için drand turunu yerel olarak hazırlar; canlı talep varsa zincirde yayınlar. Henüz yayınlanmamış epoch için ücret escrow'a alınabilir. Hedef blok `max(requestBlock, epochCommitBlock + 1)` olur. Epoch kaynağı iki ağda da yalnız drand'dır (katalog `[11]`): Arc Testnet'te 11319. epochtan, Arc Mainnet'te 12448. epochtan itibaren. Bu epochlardan öncekiler imzalı API kayıtlarıyla yayınlandı (recipe 0-10) ve replay ile hâlâ doğrulanır. Yedek kaynak yoktur: drand turu alınamazsa epoch yayınlanmaz ve fulfilled olmayan istek süre dolunca iade edilebilir.
- Süre, isteğin zincire dahil edildiği blok zamanından başlar. Geçerli VRF proof'u en geç `başlangıç + 60 saniye` zamanında zincirde kabul edilmelidir. Mempool'a gönderim yeterli değildir.
- Süre dolduğunda fulfilled olmayan isteği herkes iade ettirebilir. Ödeme yalnız sabit `_refundAddress` adresine gider. RNG ücreti iade edilir; ağ gas'ı ve uygulamanın diğer ödemeleri dahil değildir.
- Transfer başarısızsa aynı adres adına tamamen fonlanmış refund credit oluşur. Bu adres krediyi daha sonra başka bir alıcıya çekebilir.
- Proof kabulünden sonraki callback hatası ücret iadesi sağlamaz. Randomness callback'i yalnız aynı kayıtlı word ile tekrar denenir; yeni sonuç ve ikinci servis ücreti oluşmaz.
- Keeper payı owner tarafından yönetilir. Başlangıç ayarı iki ağda da %50'dir (`keeperFeeBps` 5000). Arc Testnet'te oran %50'dir; Arc Mainnet'te owner 2026-09-18'den beri %60 uygulamaktadır (deployment manifestindeki `configurationUpdates`). Her ağda güncel oranı `keeperFeeBps()` ile okuyun.

## İade bildirimi: `onRefund(requestId)`

Bu kaynak sürüm opsiyonel refund callback desteğini ekler. Eski implementation kullanan deployment'larda hook etkin değildir; kullanılan proxy implementation'ını ve deployment kaydını doğrulayın.

İade/credit muhasebesi kesinleştirildikten sonra coordinator, isteğin **consumer kontratına** `onRefund(uint256 requestId)` çağrısı yapar. Bildirim `_refundAddress` adresine yönlendirilmez. Yalnız request ID gönderilir. Bu bildirim isteğin kapandığını ve iadenin ödendiğini **veya krediye yazıldığını** söyler; consumer'ın kendisine para geldiği anlamına gelmez.

`D20VRFConsumer` çağrıyı yalnız kendi coordinator'ından kabul eder. Uygulama opsiyonel `_onRefund(uint256 requestId)` metodunu override edebilir. İstek/uygulama kaydı eşleştirmesini kontrol edin ve aynı kaydı ikinci kez kapatmayın. Uygulamanın kilitlediği varlıkları çözme ve kendi ücret iadeleri uygulamanın sorumluluğudur.

İlk bildirim 100.000 gas ile sınırlıdır. Revert, gas tüketimi veya büyük dönüş verisi ücret iadesini iptal etmez. Eski consumer hook'u uygulamıyorsa da ücret iadesi tamamlanır. `RefundCallbackAttempted` sonucu ayrı event'tir; `refundCallbackDelivered(requestId)` bildirim durumudur.

Başarısız bildirim `retryRefundCallback(requestId, gasLimit)` ile 100.000–1.000.000 gas aralığında tekrar denenebilir. Bu çağrı yeniden para göndermez, randomness üretmez ve başarılı bildirim ikinci kez çalıştırılamaz. Refund ve retry işlemlerinde yeterli dış gas verilmelidir; gas estimation callback bütçesini de hesaba katar.

Ortak reentrancy kilidi, para transferi veya bildirim içinden tekrar refund, credit withdrawal, callback retry veya yeni randomness request açılmasını engeller. Yeni bağımsız işlem açmak gerekiyorsa uygulama bunu ayrı bir işlemle yürütmelidir.
