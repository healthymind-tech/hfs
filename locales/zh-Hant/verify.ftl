# 「先設定、再驗證」報告（crates/auth/src/verify.rs）。
# 每個步驟帶有代碼：`verify-{code}` 為訊息，`verify-{code}-fix` 為修正方式。
# 鍵值須與 en/verify.ftl 一致。

## 步驟標題

verify-step-mode = 驗證模式
verify-step-input = 輸入格式
verify-step-outbound = 對外連線政策
verify-step-connect = 連線
verify-step-discovery = 探索文件
verify-step-issuer = 簽發者
verify-step-jwks-url = JWKS 網址
verify-step-jwks = 簽章金鑰（JWKS）
verify-step-algorithms = 演算法
verify-step-smart = SMART 功能
verify-step-audience = 受眾
verify-step-audience-shared = 與其他端點共用受眾
verify-step-token = Token 驗證
verify-step-metadata = 能力聲明
verify-step-lookup = 代碼查詢
verify-step-expand = 值集展開
verify-step-latency = 回應時間
verify-step-counts = 資源數量
verify-step-write = 寫入權限
verify-step-incremental = 增量同步

## 共用

verify-skipped-after-failure = 未檢查：步驟 { $after } 先失敗了。
verify-skipped-after-failure-fix = 請先修正該步驟，再重新驗證。

verify-mode-off = 驗證已關閉：不需任何憑證，token 一律忽略。
verify-token-mode-off = 驗證已關閉，因此未檢查 token。

verify-input-ok = 所有網址格式正確。
verify-input-required = 必須填寫 { $field }。
verify-input-required-fix = 請填寫 { $field }。
verify-input-not-url = 「{ $value }」不是網址。
verify-input-not-url-fix = 請輸入以 https:// 開頭的完整網址，例如 https://idp.example.org/realms/fhir。
verify-input-plain-http = { $field } 使用未加密的 http：金鑰與 token 會以明文傳輸。
verify-input-plain-http-fix = 若伺服器支援，請改用 https。

verify-outbound-trusted = 由維運人員設定：不受對外連線政策限制。
verify-outbound-ok = 對外連線政策允許連線至 { $hosts }。
verify-outbound-blocked = 對外連線政策不允許連線至 { $host }。
verify-outbound-blocked-fix = 請將 { $host } 加入 HFS_OUTBOUND_ALLOW 並重新啟動伺服器，或改用公開的 https 位址。
verify-outbound-plain-http = { $host } 使用未加密的 http，僅允許用於 HFS_OUTBOUND_ALLOW 中的主機。
verify-outbound-plain-http-fix = 請改用 https，或將 { $host } 加入 HFS_OUTBOUND_ALLOW 並重新啟動伺服器。
verify-outbound-credentials = 網址中含有使用者名稱或密碼。
verify-outbound-credentials-fix = 請移除 user:password@ 部分，並在專屬欄位輸入憑證。

verify-connect-ok = 已連線至 { $host }。
verify-connect-dns = 主機名稱 { $host } 不存在（DNS 查詢失敗）。
verify-connect-dns-fix = 請檢查主機名稱拼寫，並確認本伺服器能解析該名稱。
verify-connect-refused = { $host } 拒絕連線：該位址與連接埠沒有服務在監聽。
verify-connect-refused-fix = 請檢查連接埠，並確認伺服器正在執行。
verify-connect-timeout = { $host } 未在時限內回應。
verify-connect-timeout-fix = 請確認伺服器已啟動，且沒有防火牆阻擋本伺服器。
verify-connect-tls = 與 { $host } 的安全連線失敗（TLS：憑證或協定問題）。
verify-connect-tls-fix = 請檢查憑證（是否過期、自簽或主機名稱不符），或確認該連接埠是否提供 https。
verify-connect-failed = 無法連線至 { $host }。
verify-connect-failed-fix = 請檢查網址，以及本伺服器與 { $host } 之間的網路。

## 端點驗證

verify-discovery-ok = 已找到探索文件。
verify-discovery-404 = { $url } 沒有探索文件。
verify-discovery-404-fix = 請檢查簽發者路徑（Keycloak：https://host/realms/<realm>）。
verify-discovery-status = 探索文件網址回應 HTTP { $status }。
verify-discovery-status-fix = 請檢查簽發者網址，並確認身分提供者運作正常。
verify-discovery-html = 該網址回應的是網頁，而不是 JSON。
verify-discovery-html-fix = 簽發者可能是登入頁面或代理伺服器。請完全依照身分提供者所述輸入簽發者。
verify-discovery-not-json = { $url } 的回應不是 JSON 探索文件。
verify-discovery-not-json-fix = 請檢查簽發者路徑；身分提供者必須在其下提供 .well-known/openid-configuration。

verify-issuer-ok = 身分提供者聲明的簽發者相同。
verify-issuer-mismatch = 身分提供者聲明其簽發者為 { $stated }，您輸入的是 { $entered }。所有 token 都會被拒絕。
verify-issuer-mismatch-fix = 請使用 { $stated } 作為簽發者。
verify-issuer-not-stated = 探索文件未聲明簽發者。
verify-issuer-not-stated-fix = 請確認網址指向 OpenID Connect 身分提供者。

verify-jwks-url-ok = JWKS 網址與簽發者公布的一致。
verify-jwks-url-from-discovery = JWKS 網址留空；簽發者公布的是 { $discovered }。
verify-jwks-url-from-discovery-fix = 可保持留空，或從探索文件填入。
verify-jwks-url-differs = JWKS 網址為 { $configured }，但簽發者公布的是 { $discovered }。
verify-jwks-url-differs-fix = 除非您確知兩者不同的原因，否則請使用公布的 JWKS 網址。
verify-jwks-url-missing = 沒有 JWKS 網址：欄位為空，簽發者也未公布。
verify-jwks-url-missing-fix = 請輸入身分提供者的 JWKS 網址。

verify-jwks-ok = { $count } 把簽章金鑰：{ $kids }（{ $algs }）。
verify-jwks-empty = JWKS 中沒有簽章金鑰。
verify-jwks-empty-fix = 請在身分提供者中建立或啟用簽章金鑰。
verify-jwks-no-usable-key = JWKS 中有 { $count } 把金鑰，但沒有帶金鑰識別碼（kid）的簽章金鑰。
verify-jwks-no-usable-key-fix = 請公布帶有 "use": "sig" 與 "kid" 的簽章金鑰。
verify-jwks-not-json = JWKS 網址未回應 JSON 金鑰集。
verify-jwks-not-json-fix = 請檢查 JWKS 網址；它必須回傳含有 "keys" 清單的 JSON 物件。
verify-jwks-404 = { $url } 沒有 JWKS。
verify-jwks-404-fix = 請檢查 JWKS 網址，或留空以使用簽發者公布的網址。
verify-jwks-status = JWKS 網址回應 HTTP { $status }。
verify-jwks-status-fix = 請檢查 JWKS 網址，並確認身分提供者運作正常。
verify-jwks-unreachable = 無法取得 { $url } 的 JWKS（{ $reason }）。
verify-jwks-unreachable-fix = 請檢查 JWKS 網址的主機、連接埠及對外連線政策。
verify-jwks-foreign-keys = { $configured } 的金鑰不屬於此簽發者：其金鑰識別碼都不在 { $discovered } 中。Token 將無法通過簽章檢查。
verify-jwks-foreign-keys-fix = 此 JWKS 屬於另一個 realm 或身分提供者。請改用 { $discovered }。

verify-algorithms-ok = 以 { $algorithms } 簽章的 token 可以驗證。
verify-algorithms-unsupported = 接受的演算法（{ $configured }）都不受支援。支援的有：{ $supported }。
verify-algorithms-unsupported-fix = 請從 { $supported } 中選擇演算法。
verify-algorithms-no-match = 接受的演算法（{ $configured }）與任何金鑰（{ $keys }）都不相符。
verify-algorithms-no-match-fix = 請接受 { $keys }，或讓身分提供者改用 { $configured } 之一簽章。

verify-smart-ok = 已公布授權與 token 端點並支援 PKCE（授權類型：{ $grants }）。
verify-smart-no-token = 探索文件沒有 token 端點（缺少：{ $missing }）。
verify-smart-no-token-fix = 用戶端無法從此簽發者取得 token。請檢查身分提供者的設定。
verify-smart-no-authorize = 探索文件沒有授權端點（缺少：{ $missing }）。
verify-smart-no-authorize-fix = 應用程式無法讓使用者登入；只有後端服務（用戶端憑證）可以使用。
verify-smart-no-pkce = 未公布 PKCE S256（缺少：{ $missing }）。
verify-smart-no-pkce-fix = 請在身分提供者中為用戶端啟用 PKCE（S256）；SMART 應用程式需要它。
verify-smart-no-client-credentials = 未公布 client_credentials 授權類型（缺少：{ $missing }）。
verify-smart-no-client-credentials-fix = 若需要後端服務連線，請啟用服務帳號／用戶端憑證。

verify-audience-ok = Token 必須是為 { $audience } 簽發的。
verify-audience-missing = 未設定受眾：此簽發者的任何 token 都會被接受，包括為其他服務簽發的 token。
verify-audience-missing-fix = 請將受眾設為只有本端點 token 才帶有的值，並在身分提供者中加入受眾對應器（audience mapper）。
verify-audience-shared = 端點 { $other } 使用相同的簽發者與受眾：其中一個端點的 token 也會被另一個接受。
verify-audience-shared-fix = 請為每個端點設定各自的受眾。

verify-token-ok = Token 已被接受：主體 { $sub }，範圍 { $scopes }。
verify-token-audience = Token 的受眾為 { $got }；此端點要求 { $expected }。
verify-token-audience-fix = 請在身分提供者中加入受眾對應器，或修改受眾。
verify-token-expired = Token 已於 { $expired_at } 過期。
verify-token-expired-fix = 請重新取得 token 後再試一次。
verify-token-signature = Token 的簽章與簽章金鑰不符。
verify-token-signature-fix = 此 token 可能由另一個 realm 或身分提供者簽發。請檢查簽發者與 JWKS 網址。
verify-token-issuer = Token 的簽發者為 { $got }；此端點要求 { $expected }。
verify-token-issuer-fix = 請向 { $expected } 取得 token，或修正簽發者。
verify-token-algorithm = Token 以 { $alg } 簽章，此端點不接受。
verify-token-algorithm-fix = 請將 { $alg } 加入接受的演算法。
verify-token-malformed = 這不是 JWT。
verify-token-malformed-fix = 請貼上 access token 本身（以句點分隔的三段），不要加引號。
verify-token-not-yet-valid = Token 尚未生效。
verify-token-not-yet-valid-fix = 請檢查本伺服器與身分提供者的時鐘。
verify-token-missing-claim = Token 缺少必要的宣告 { $claim }。
verify-token-missing-claim-fix = 請設定身分提供者在 token 中包含 { $claim }。
verify-token-refused = Token 被拒絕。
verify-token-refused-fix = 請檢查憑證與 token 內容。

## 術語伺服器

verify-metadata-ok = { $software } { $version } 已回應（FHIR { $fhir }）。
verify-metadata-404 = { $url } 沒有能力聲明。
verify-metadata-404-fix = 這可能不是 FHIR 基礎網址。術語伺服器通常位於 /fhir 或 /tx 等路徑下。
verify-metadata-unauthorized = 伺服器拒絕了 /metadata（HTTP { $status }）。
verify-metadata-unauthorized-fix = 請輸入伺服器接受的 token。
verify-metadata-status = /metadata 回應 HTTP { $status }。
verify-metadata-status-fix = 請檢查網址，並確認伺服器運作正常。
verify-metadata-not-fhir = 該網址未回應 FHIR 能力聲明。
verify-metadata-not-fhir-fix = 請確認網址是伺服器的 FHIR 基礎網址。
verify-token-accepted = 伺服器接受此憑證。
verify-token-not-checkable = 伺服器不需憑證也會回應，因此無法檢查 token。
verify-lookup-ok = LOINC 8867-4 查得「{ $display }」。
verify-lookup-missing-system = 伺服器不認得 { $system }。
verify-lookup-missing-system-fix = 請將術語（例如 LOINC）匯入術語伺服器。
verify-lookup-failed = 代碼查詢失敗。
verify-lookup-failed-fix = 請檢查伺服器日誌；驗證功能需要 $lookup 正常運作。
verify-expand-ok = 行政性別值集展開為 { $count } 個代碼。
verify-expand-missing = 伺服器無法展開 { $valueset }。
verify-expand-missing-fix = 請將 FHIR 核心值集載入術語伺服器。
verify-expand-failed = 值集展開失敗。
verify-expand-failed-fix = 請檢查伺服器日誌；驗證功能需要 $expand 正常運作。
verify-latency-ok = 最慢的回應花了 { $ms } 毫秒（逾時設定 { $timeout_ms } 毫秒）。
verify-latency-high = 最慢的回應花了 { $ms } 毫秒，超過逾時設定 { $timeout_ms } 毫秒的一半。
verify-latency-high-fix = 請提高逾時設定，或改用離本伺服器較近的術語伺服器。

## 同步對象

verify-metadata-version-mismatch = 該伺服器使用 FHIR { $got }；此工作要求 { $expected }。
verify-metadata-version-mismatch-fix = 請選擇相同 FHIR 版本的伺服器，或修改工作的 FHIR 版本。
verify-token-issued = Token 端點已核發 access token。
verify-token-invalid-client = Token 端點拒絕了此用戶端。
verify-token-invalid-client-fix = 請檢查用戶端 ID 與用戶端密鑰。
verify-token-invalid-scope = Token 端點拒絕了所要求的範圍。
verify-token-invalid-scope-fix = 請只要求此用戶端獲准的範圍，例如 system/*.rs。
verify-token-endpoint-404 = { $url } 沒有 token 端點。
verify-token-endpoint-404-fix = 請檢查 token 網址。
verify-token-endpoint-unreachable = 無法連線至 token 端點（{ $reason }）。
verify-token-endpoint-unreachable-fix = 請檢查 token 網址的主機與連接埠，以及對外連線政策。
verify-token-bad-response = Token 端點回應 HTTP { $status }，但沒有 access token。
verify-token-bad-response-fix = 請確認 token 網址是 OAuth token 端點。
verify-token-error = Token 端點回應 { $error }。
verify-token-error-fix = 請檢查身分提供者中的用戶端設定。
verify-token-required = 伺服器需要驗證。
verify-token-required-fix = 請選擇驗證方式並輸入憑證。
verify-token-not-needed = 伺服器不需憑證即會回應。
verify-counts-ok = { $summary }
verify-counts-no-types = 未選擇資源類型：會複製所有類型，因此未計算數量。
verify-counts-unknown-type = 伺服器不提供 { $type }。
verify-counts-unknown-type-fix = 請從資源類型中移除 { $type }，或檢查伺服器的 FHIR 版本。
verify-counts-refused = 伺服器拒絕了搜尋（HTTP { $status }）。
verify-counts-refused-fix = 請確認憑證有權讀取這些資源類型。
verify-write-ok = 伺服器對所選類型公布了建立與更新功能。
verify-write-not-advertised = 伺服器未對 { $types } 公布建立或更新功能。
verify-write-not-advertised-fix = 寫入這些類型可能會被拒絕。請檢查伺服器的能力聲明及用戶端權限。
verify-incremental-ok = 伺服器支援 _lastUpdated 或歷史紀錄：之後的執行只會讀取變更。
verify-incremental-unsupported = 伺服器未對 { $types } 公布 _lastUpdated 或歷史紀錄。
verify-incremental-unsupported-fix = 每次執行可能都要讀取全部資料；請選擇較長的間隔。
