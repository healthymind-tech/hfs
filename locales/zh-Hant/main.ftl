# Helios FHIR Server — UI 訊息目錄
# 語系：繁體中文（台灣）(zh-Hant)
#
# 鍵名與 `en/main.ftl`（來源語系）完全相同。缺少的鍵依 docs/multi-language.md
# 所述的回退鏈回到英文。FHIR 資源類型、操作名（$expand 等）、參數名（_count 等）
# 與 HTTP 動詞依業界慣例保留英文原文。

## Brand / shared terms

-app-name = Helios FHIR Server
-org-name = Helios Software

## Page chrome

app-title = { -app-name }
app-tagline = 快速、支援多版本的FHIR伺服器

nav-dashboard = 儀表板
nav-terminology = 術語
nav-terminology-new-window = 術語（在新分頁開啟）
nav-resources = 資源
nav-settings = 設定
nav-signout = 登出

## Language switcher

language-label = 語言
language-en = 英文
language-es = 西班牙文
language-de = 德文
language-zh-hant = 繁體中文
user-menu-label = 帳戶選單
user-anonymous = 匿名使用者
user-local-hint = 已停用身分驗證
user-logout = 登出

## Home / landing page

home-lede = 由伺服器端渲染、以HTMX為主的介面。此面板會以HTML片段的方式重新整理。

## Status panel

status-last-checked = 最後檢查時間：{ $timestamp }

## Dashboard / health

dashboard-heading = 伺服器儀表板
health-status-ok = 所有系統運作正常
health-status-degraded = 部分系統已降級
health-uptime = 運作時間：{ $duration }

# Pluralized count — every locale must supply the plural categories its
# grammar requires (CLDR rules; Fluent selects the branch automatically).
resource-count = { $count ->
    [one] { $count }筆資源
   *[other] { $count }筆資源
}

## Terminology browsing

terminology-heading = 術語伺服器
terminology-lede = 將HFS連接至FHIR術語伺服器。
terminology-configured-heading = 已設定術語伺服器
terminology-configured-body = HFS_TERMINOLOGY_SERVER指向有效的伺服器URL。
terminology-configured-open = 開啟術語伺服器
terminology-invalid-heading = HFS_TERMINOLOGY_SERVER無效。
terminology-invalid-body = 請使用含主機名稱的絕對HTTP或HTTPS URL。可包含路徑與結尾斜線，但不可包含帳密、查詢字串或片段（fragment）。
terminology-invalid-note = 更新環境變數後，重新啟動HFS。
terminology-setup-heading = 連接術語伺服器
terminology-setup-body = 將HFS_TERMINOLOGY_SERVER設為HFS應使用的FHIR術語伺服器基礎URL。
terminology-setup-note = 在啟動HFS的環境中設定此變數，然後重新啟動伺服器。
terminology-search-label = 搜尋CodeSystem與ValueSet
terminology-search-placeholder = 例如：73211009、「diabetes」、http://snomed.info/sct
terminology-display-language = 顯示語言
terminology-no-results = 找不到符合的概念。

## Common actions

action-search = 搜尋
action-save = 儲存
action-cancel = 取消
action-retry = 重試

## Errors (mirrors OperationOutcome text; see docs/multi-language.md §5)

error-not-found = 找不到要求的資源。
error-unauthorized = 你沒有執行此操作的權限。
error-generic = 發生錯誤，請再試一次。

## Generic "not found" page (#835): a full-page 404 inside the shell, distinct
## from error-not-found above (OperationOutcome text for API responses).

not-found-title = 找不到頁面
not-found-text = 此頁面不存在，或你無法存取。

## Dashboard shell (Figma "Dashboard V1.1")

nav-section-work = 工作
nav-section-batch-data = 批次與資料
nav-section-sql-on-fhir = SQL on FHIR
nav-section-server = 伺服器
nav-section-tools = 工具

nav-home = 首頁
nav-search = 搜尋
nav-resource-editor = 資源編輯器
nav-history-versions = 歷程與版本
nav-compartments = Compartment
nav-batch-transaction = 批次／交易
nav-import = 匯入
nav-export = 匯出
nav-sql-view-definitions = ViewDefinition
nav-sql-queries = SQL查詢
nav-sql-views = SQL檢視表
nav-sql-export = SQL匯出
nav-capability-conformance = 能力與一致性
nav-search-parameters = 搜尋參數
nav-subscriptions = 訂閱
nav-tenants = 租戶

## Tenant maintenance (/ui/tenants)

tenants-title = 租戶維護
tenants-lede = 建立、檢視與刪除此伺服器用來隔離資料的租戶（Tenant）。
tenants-unavailable = 此儲存後端不提供租戶登錄表。
tenants-stat-total = 租戶總數
tenants-stat-total-sub = { $count ->
    [one]已登錄{ $count }個
   *[other]已登錄{ $count }個
}
tenants-stat-resources = 已儲存資源
tenants-stat-resources-sub = 所有租戶合計
tenants-search-placeholder = 依名稱或租戶ID搜尋…
tenants-add = 新增租戶
tenants-add-title = 新增租戶
tenants-field-id = 租戶ID
tenants-field-id-hint = 用於API（X-Tenant-ID標頭、URL前綴、JWT claim）。
tenants-field-name = 顯示名稱（選填）
tenants-field-name-hint = 便於辨識的標籤，不用於路由。
tenants-add-submit = 建立租戶
tenants-col-tenant = 租戶
tenants-col-resources = 資源
tenants-col-created = 建立時間
tenants-col-actions = 操作
tenants-empty = 沒有符合的租戶。
tenants-unregistered = 未登錄
tenants-delete = 刪除租戶
tenants-delete-confirm = 確定要取消登錄租戶「{ $id }」嗎？除非透過API清除，否則其已儲存的資料會保留。
tenants-row-provisioning = 建立中…可能需要一點時間。
tenants-row-failed = 無法建立租戶。
tenants-dismiss = 關閉
# Submit-time errors on the Add Tenant form (#317 mirrors the API's id rules).
tenants-error-id-empty = 租戶ID不可為空。
tenants-error-id-too-long = 租戶ID超過{ $max }個字元。
tenants-error-id-reserved = 此ID保留給內部共用資源使用。
tenants-error-id-invalid-character = 租戶ID只能包含字母、數字、'-'、'_'、'.'與'/'。
tenants-error-id-empty-segment = 租戶ID不可以'/'開頭或結尾，也不可包含空的區段。
tenants-error-exists = 租戶「{ $id }」已存在。
tenants-error-provisioning = 租戶「{ $id }」正在建立中。
tenants-error-still-provisioning = 租戶「{ $id }」仍在建立中。
tenants-error-not-purged = 租戶「{ $id }」已取消登錄，但其資料「尚未」清除：{ $detail }
tenants-error-list = 無法列出租戶：{ $detail }
tenants-error-count = 無法計算資源數量：{ $detail }
tenants-field-name-placeholder = 範例醫院

tenant-heading = 租戶
tenant-all = 所有租戶
tenant-search-placeholder = 搜尋租戶

theme-toggle-system = 主題：跟隨系統
theme-toggle-light = 主題：淺色
theme-toggle-dark = 主題：深色

fhir-version = FHIR { $version }
fhir-version-heading = FHIR版本

card-resource-types = 資源類型
card-resource-types-sub = 用於{ $version }
card-stored-resources = 已儲存資源
card-stored-resources-sub = 目前租戶內
card-export-jobs = 匯出工作
card-export-jobs-sub = 執行中（{ $queued }個已排入佇列）
card-import-jobs = 匯入工作
card-import-jobs-sub = 進行中
card-jobs-unavailable = 無法使用
card-uptime = 運作時間
card-uptime-sub = 自程序啟動起算

chart-title = FHIR資源數量趨勢
chart-window = 圖表時間範圍
chart-pick-heading = 圖表顯示的資源類型
chart-pick-all = 檢視所有資源類型
chart-pick-filter = 篩選類型
chart-empty = 尚無可繪製的資料——已儲存的資源會在建立後顯示於此。
# Only for a build with no metrics provider at all. A provider that was merely
# slow gets chart-pending-note instead — the two must never be confused (#956).
chart-sample-note = 範例資料：此版本沒有即時指標提供者，因此下方圖表僅為示意，並非實際量測值。
chart-pending-note = 仍在收集此時間範圍的即時數據。數據送達前不會繪製任何內容，也不會顯示示意數字。
chart-pending-empty = 正在等待即時數據…
chart-pending-retry = 立即重試
# #1078: measured, but counted from in-memory write counters rather than an
# exact storage read. A label, not a warning.
chart-approximate-note = 約略值：由近期寫入計數得出，仍在與儲存層核對中。
# #1078: the storage backend cannot count resources at all (e.g. an S3
# primary). Shown in the chart area; the notice line below explains it.
chart-counts-unsupported = 此儲存後端無法提供資源數量。
chart-counts-unsupported-note = 此儲存後端無法計算已儲存的資源數量，因此不顯示總數或圖表。這不代表租戶是空的。
# #1078: when the figures on the page were read. $time is a UTC time,
# prefixed with the date when it is not today (e.g. "14:02:31 UTC").
chart-as-of = 資料時間：{ $time }。
# #1065: a search-index rebuild ($reindex) is running for this tenant. Stored
# resources stay readable by id, but searches can miss them until it finishes.
# $percent is a whole number; $processed and $total are resource counts.
search-index-rebuilding = 搜尋索引重建中——{ $percent }%（{ $total }筆資源中已處理{ $processed }筆）。重建完成前，搜尋可能遺漏已儲存的資源。
# The same, before the rebuild has counted the resources it will process.
search-index-rebuilding-counting = 搜尋索引重建中。重建完成前，搜尋可能遺漏已儲存的資源。
# #1125: the tenant's most recent search-index rebuild ended with resources
# left unindexed. $errors is a resource count; $job is the rebuild job id.
search-index-rebuild-failed = { $errors ->
    [one]上次搜尋索引重建留下1筆資源未建立索引。在重建成功之前，搜尋會遺漏該資源；GET $reindex-status/{ $job }可查看是哪一筆。
   *[other]上次搜尋索引重建留下{ $count }筆資源未建立索引。在重建成功之前，搜尋會遺漏這些資源；GET $reindex-status/{ $job }可列出是哪些。
  }
# The same, when the rebuild failed as a whole before naming any resource.
search-index-rebuild-failed-job = 上次搜尋索引重建在完成前就失敗了。在重建成功之前，搜尋可能遺漏已儲存的資源；GET $reindex-status/{ $job }可查看原因。
chart-table-toggle = 以表格檢視
chart-table-when = 時間
chart-focus-series = 聚焦此序列
chart-unfocus-series = 顯示所有序列

## Footer

footer-copyright = © { $year } { -org-name }

## History & Versions (#236)

history-heading = 歷程與版本
history-lede = 比較資源的兩個版本。儲存層具備完整版本管理；此頁透過一般的_history與vread API讀取。
history-type-label = 資源類型
history-id-label = 資源ID
history-id-placeholder = 資源ID
history-load = 載入
history-tabs-label = 歷程範圍
history-tab-instance = 單一實例
history-tab-type = 類型動態
history-tab-system = 系統動態
history-versions-label = 版本
history-current = 目前
history-from = 從
history-to = 到
history-show-metadata = 顯示中繼資料變更
history-empty = 載入資源後，選擇兩個版本進行比較。
history-load-error = 無法載入該資源的歷程。
history-not-found = 該資源沒有歷程——請確認類型與ID。
history-diff-heading = { $from }
history-metadata-hidden = { $count ->
    [one]已隱藏{ $count }項中繼資料變更
   *[other]已隱藏{ $count }項中繼資料變更
}
history-textual = 顯示完整文字差異
history-only-metadata = 這兩個版本之間只有中繼資料不同。
history-identical = 這兩個版本完全相同。
history-deleted = { $version }是刪除操作——沒有可比較的內容。
history-parse-error = 無法將這些版本解析為JSON。
## Saved queries (#234)

nav-saved-queries = 已儲存的查詢

queries-heading = 已儲存的查詢
queries-lede = 依資源類型保存FHIR搜尋查詢，並依最近執行時間排序。查詢儲存在你的使用者設定中，可跨裝置同步。
queries-add-heading = 儲存查詢
queries-type-label = 資源類型
queries-type-placeholder = 例如：Patient
queries-name-label = 名稱
queries-name-placeholder = 例如：波士頓的Smith家族
queries-query-label = 查詢字串
queries-query-placeholder = 例如：name=smith&address-city=Boston
queries-empty = 尚無已儲存的查詢。請在上方儲存一筆開始使用。
queries-never-run = 尚未執行
queries-run = 執行
queries-rename = 重新命名
queries-delete = 刪除
queries-rename-prompt = 新名稱
queries-confirm-delete = 確定要刪除「{ $name }」嗎？
queries-unavailable = 無法使用已儲存的查詢：此伺服器的儲存後端不支援個別使用者設定。

## SearchParameter viewer (#238)

sp-heading = 搜尋參數
sp-lede = 瀏覽此伺服器解析搜尋時使用的參數，可依基礎資源類型篩選。已儲存的參數可建立、編輯與刪除；登錄表會依租戶套用變更。
sp-version-label = FHIR版本
sp-degraded = 目前無法從此伺服器載入搜尋參數——對/SearchParameter的自我呼叫失敗（啟用身分驗證時，通常表示對外服務權杖遺失或無效）。下次請求時頁面會自動重試。
sp-rail-label = 資源篩選
sp-rail-search = 篩選類型
sp-rail-recent = 最近使用
sp-rail-types = 資源類型
sp-rail-all = 所有類型
sp-facet-type = 類型
sp-facet-type-label = 依參數類型篩選
sp-facet-source = 來源
sp-facet-source-label = 依來源篩選
sp-source-embedded = 內建
sp-source-stored = 已儲存
sp-source-config = 設定檔
sp-chip-conflict = 衝突
sp-chip-overrides = 覆寫規格
sp-chip-shadowed = 被遮蔽
sp-col-code = 代碼
sp-col-type = 類型
sp-col-base = 基礎類型
sp-col-expression = 表達式
sp-col-source = 來源
sp-total = { $count }個參數
sp-pagination-label = 頁數
sp-page-prev = 上一頁
sp-page-next = 下一頁
sp-detail-label = 參數詳細資料
sp-detail-empty = 尚未選擇參數
sp-detail-empty-hint = 選擇一列以檢視其定義、表達式，以及在登錄表中的解析方式。
sp-detail-readonly = 規格參數（由資料檔編譯內建）——唯讀。
sp-field-url = 正準URL (canonical URL)
sp-field-name = 名稱
sp-field-status = 狀態
sp-field-base = 基礎資源類型
sp-field-expression = FHIRPath表達式
sp-field-description = 說明
sp-field-target = 目標類型
sp-field-components = 組成元件
sp-status-hint = 載入器在載入時會將規格中的draft狀態提升為active。
sp-note-conflict = 與{ $url }在同一來源內有重複的(base, code)——登錄表會拒絕此衝突(DuplicateCode)。
sp-note-overrides = 在(base, code)上覆寫{ $url }：已儲存的定義優先於規格參數，因此由此參數解析搜尋。登錄表會記錄一筆WARN並列出兩個URL。
sp-note-shadowed = 在(base, code)上被{ $url }遮蔽：優先順序較高的來源會解析此位置的搜尋。
sp-note-empty-expression = 表達式為空：擷取器不會建立任何索引列，因此對此參數的所有搜尋都會悄悄回傳空結果。
sp-note-no-target = Reference類型參數未指定目標類型：鏈結搜尋無法解析被參照的類型。
sp-note-choice-type = Choice型別表達式：擷取器會先將ofType(T)／as T改寫為具體元素（例如valueQuantity），再對原始儲存的JSON求值。
sp-new = 新增搜尋參數
sp-edit = 編輯
sp-delete = 刪除
sp-delete-confirm = 確定要刪除此已儲存的搜尋參數嗎？登錄表重新整理後，使用它的搜尋將不再比對到結果。
cmp-new = 新增Compartment定義
cmp-edit = 編輯
cmp-delete = 刪除
cmp-delete-confirm = 確定要刪除此Compartment定義嗎？其Compartment路由將不再解析。
crud-delete-failed = 無法刪除此項目。

## Compartment viewer & tester (#237)

cmp-heading = Compartment
cmp-lede = 本伺服器用來路由/{"{"}compartment{"}"}/{"{"}id{"}"}/{"{"}type{"}"}請求的Compartment定義，以及一個測試工具，可回答：此類型是否屬於此Compartment、經由哪些參數，以及伺服器實際執行的搜尋為何？
cmp-rail-label = Compartment定義
cmp-rail-heading = Compartment
cmp-degraded = 目前無法從本伺服器載入Compartment定義——對/CompartmentDefinition的自我呼叫失敗（啟用驗證時，通常表示對外服務權杖遺失或無效）。頁面會在下一次請求時重試。
cmp-rail-note = 定義是儲存的資源，啟動時由FHIR規格植入。此處的編輯與刪除僅限於目前租戶。
cmp-tabs-label = Compartment區段
cmp-tab-definition = 定義
cmp-tab-members = 成員
cmp-tab-tester = 測試工具
cmp-field-code = 代碼
cmp-field-status = 狀態
cmp-field-url = 正準URL
cmp-field-version = 版本
cmp-field-publisher = 發行者
cmp-field-description = 說明
cmp-field-search = search
cmp-field-experimental = experimental
cmp-search-why = 若為關閉，表示此Compartment不會解析任何Compartment路由。
cmp-on = 開啟
cmp-off = 關閉
cmp-yes = 是
cmp-no = 否
cmp-readonly-note = 唯讀：這些值來自編譯進伺服器的規格定義。
cmp-filter-members = 成員
cmp-filter-all = 全部類型
cmp-filter-excluded = 已排除
cmp-member = 成員
cmp-excluded = 已排除
cmp-tester-id = ID
cmp-tester-target = 目標類型（或*）
cmp-tester-run = 測試
cmp-result-member = ✓ 成員 — 經由{ $params }
cmp-result-flat = // 等效的平面搜尋
cmp-result-member-note = 伺服器會將Compartment路由解析為對該類型參照參數的這個搜尋。
cmp-result-self = ✓ 成員 — Compartment資源本身（{"{"}def{"}"}）
cmp-result-self-note = Compartment實例理所當然屬於自身的Compartment；路由會直接讀取該資源。
cmp-result-notmember = ✕ { $type }不是此Compartment的成員
cmp-result-notmember-note = 對於非Compartment成員的類型，伺服器會回傳404並附上OperationOutcome。
cmp-result-fanout = 展開至{ $count }個成員類型
cmp-result-fanout-note = 已排除的類型會被略過而非視為失敗——展開時會捨棄非成員類型，不會報錯。
queries-builder-heading = 搜尋建構器
queries-url-label = FHIR搜尋URL
queries-url-placeholder = GET /Patient?name=smith&birthdate=ge1980-01-01
queries-builder-hint = 可直接編輯GET URL，或透過下方各列編輯——兩者會保持同步。「執行」會在此執行搜尋並記錄到「最近」；為它命名即可保留在已儲存清單中。
queries-recent = 最近
queries-recent-heading = 最近的搜尋
queries-recent-empty = 尚無最近的搜尋——執行一次即會記錄在此。
queries-invalid-url = 請輸入像GET /Patient?name=smith這樣的搜尋——資源類型取自路徑。
queries-invalid-fhir-escape = 此查詢含有無效的FHIR跳脫字元。請先修正跳脫的值，再以視覺化方式編輯。

queries-conditions = 條件
queries-add-condition = 新增條件
queries-includes = 包含項目
queries-result-controls = 結果控制
queries-remove = 移除
queries-match-is = 是
queries-or = + 或
plain-pill = 白話說明
plain-find = 尋找{"{type}"}紀錄
plain-clause = {"{path}"} {"{verb}"} {"{value}"}
plain-clause-no-value = {"{path}"} {"{verb}"}
plain-and = 且
plain-or = 或
plain-arrow = 的
plain-has = 有相關的{"{type}"}，其{"{param}"} {"{verb}"} {"{value}"}
plain-has-no-value = 有相關的{"{type}"}，其{"{param}"} {"{verb}"}
plain-include = 並一併回傳每個{"{type}"}的{"{param}"}{"{target}"}
plain-revinclude = 另加上所有{"{param}"}指向此處的{"{type}"}
plain-iterate = （遞迴）
plain-count = 每頁顯示{"{n}"}筆
plain-unknown-param = 「{"{param}"}」不是{"{type}"}的搜尋參數
plain-sort = 依{"{sort}"}排序
plain-verb-is = 是
plain-verb-contains = 包含
plain-verb-exact = 完全等於
plain-verb-missing = 存在/不存在
plain-verb-missing-true = 不存在
plain-verb-missing-false = 存在
plain-verb-not = 不是
plain-verb-text = 文字符合
plain-verb-in = 屬於值集
plain-verb-not-in = 不屬於值集
plain-verb-identifier = 識別碼為
plain-verb-of-type = 識別碼類型為
plain-verb-ge = 不早於
plain-verb-le = 不晚於
plain-verb-gt = 晚於
plain-verb-lt = 早於
plain-verb-ne = 不等於
plain-verb-eq = 等於
plain-verb-sa = 起始晚於
plain-verb-eb = 結束早於
plain-verb-ap = 約為
queries-related-heading = 包含相關資料
queries-related-sub = 將相關聯的資源加入結果。
queries-related-add-include = 包含此資源所指向的資源
queries-related-add-revinclude = 包含指向此處的資源
queries-iterate = 遞迴
queries-sort-label = 排序
queries-sort-default = 預設
queries-sort-recent = 最新優先
queries-sort-oldest = 最舊優先
queries-sort-id = ID
queries-modify-heading = 修飾詞
queries-mod-exact = 完整值（含大小寫與重音）
queries-mod-contains = 文字中任意位置符合
queries-mod-missing = 欄位存在／不存在
queries-mod-text = 進階文字處理
queries-mod-not = 所有值皆不符合
queries-mod-above = 此項或其上層
queries-mod-below = 此項或其下層
queries-mod-in = 值集的成員
queries-mod-not-in = 非值集的成員
queries-mod-identifier = 以識別碼比對參照
queries-mod-of-type = 比對識別碼的類型、系統與值
queries-chain-into = 依被參照資源的屬性篩選
queries-chain-any-target = 任一
queries-has-pill = 有相關的
queries-has-type-placeholder = 資源類型
queries-has-via = 連結參數
queries-has-where = 且其
queries-add-has = ⧉ 篩選連結至此處的資源
queries-param-placeholder = 參數
queries-param-none = 沒有符合的參數
queries-param-unknown = 不是{"{type}"}的搜尋參數。請從清單中選擇。
queries-value-placeholder = 值
queries-results = 結果
queries-results-total = { $count }筆結果
queries-results-total-partial = { $count }+ 筆結果
queries-results-included = 已包含{ $count }筆
queries-results-empty = 沒有結果。
queries-searching = 搜尋中…
queries-search-elapsed = 已耗時{ $seconds }秒
queries-search-cancel = 取消
queries-search-slow = 這次搜尋比預期花更久時間。可以繼續等待或取消。
queries-search-keep-waiting = 繼續等待
queries-results-previous = 先前的結果
queries-col-updated = 更新時間
queries-copy-id = 複製ID
queries-copied = 已複製
queries-prev = 上一頁
queries-next = 下一頁
queries-results-fetch-error = 無法從{ $origin }載入結果。請檢查HFS_BASE_URL後再試一次。

queries-rail-heading = 資源類型
queries-rail-filter = 篩選類型

## Search — natural language & visual builder (#255)

search-heading = 搜尋
search-lede = 描述想找的內容，或手動建構查詢。兩種方式都會產生一段可以閱讀、修正並執行的FHIR搜尋查詢。
search-query-tag = 查詢
search-copy = 複製查詢

search-mode-label = 查詢撰寫方式
search-mode-nl = 自然語言
search-mode-builder = 視覺化建構器

search-nl-label = 描述搜尋內容
search-nl-placeholder = 描述想找的內容——例如：姓Smith、1980年後出生的病人
search-nl-hint = 送交語言模型的只有輸入的文字與本伺服器的搜尋參數，絕不包含病人資料。模型寫出的查詢會顯示在下方，供檢查後再執行。
search-nl-working = 轉換中…
search-nl-caveats = 請留意：
search-nl-unsupported = 本伺服器無法執行這樣的搜尋。請試著描述想找出的紀錄。

search-nl-example-1 = 65歲以上、有糖尿病診斷的女性病人
search-nl-example-2 = 最近30天的Observation，最新的排前面
search-nl-example-3 = 在Boston General仍進行中的Encounter

search-setup-heading = 可使用自然語言搜尋
search-setup-body = 將白話描述轉換成FHIR搜尋查詢。此功能需要語言模型的API金鑰——伺服器會從環境變數讀取，金鑰絕不會傳到此頁面。在設定金鑰之前，請使用下方的視覺化建構器。
search-setup-key-placeholder = 你的API金鑰
search-setup-disable = 若要完全移除此功能——包括端點、頁面與這則提示——請設定HFS_NL_SEARCH_ENABLED=false。
search-setup-docs = 閱讀操作說明

## Resource editor (#264)

editor-heading = 資源編輯器
editor-lede = 依據綱要編輯資源：可在任何深度新增綱要允許的任何元素——包括在任何可接受擴充的節點上新增擴充。
editor-title = 編輯資源
editor-view-label = 編輯方式
editor-view-form = 引導式表單
editor-view-json = JSON
editor-save = 儲存變更
editor-delete = 刪除
editor-remove = 移除此節點
editor-saved = 已儲存。
# Shared unsaved-changes tracker (#1240): the pill next to a Save button and
# the confirm shown for in-page closes (a modal, an addbox disclosure).
unsaved-changes = 未儲存的變更
unsaved-discard-confirm = 有尚未儲存的變更。要捨棄並關閉嗎？
editor-load-error = 無法載入該資源。
editor-load-not-found = 此伺服器上沒有 id 為 { $id } 的 { $type }，請確認 id 或返回清單。
editor-confirm-delete = 要刪除此資源嗎？此動作無法復原。
editor-invalid-json = 這不是有效的JSON，因此無法以表單編輯。你的文字未被更動。
editor-source-hint = 直接編輯原始內容。切回引導式表單時會重新解析。

editor-add = 新增元素
editor-add-close = 關閉
editor-add-added = 已新增
editor-add-undo = 復原
editor-add-elements = 元素
editor-add-extensions = 擴充
editor-must-support-badge = MS
editor-binding-hint = 繫結至值集——代碼來自該值集；並顯示繫結強度
editor-legend-live = 輸入時即時檢查：結構、基數、必要繫結
editor-legend-save = 儲存時檢查：約束條件與術語
# ViewDefinition's own single-line legend (#843): Save stays permissive
# there, so promising a "checked on save" pass would be misleading — this
# names everything the guided form's per-keystroke pass actually covers,
# folding in the SQL-on-FHIR-specific checks the generic validator above
# does not run.
vd-form-legend-live = 輸入時即時檢查：結構、基數、必要繫結，以及FHIRPath語法／SQL on FHIR規則
# The guided form's validity chip (#843), client-side only, while the JSON
# editor's text does not parse at all — short like the chip's other two
# states ("No issues.", "3 issues"), never the longer editor-invalid-json
# sentence.
vd-form-invalid-chip = 無效的JSON
# SQL Query / SQL View's own three-line legend (#840/#1233): Save there
# gates the SQL on FHIR Library type and the SQL attachment, not the generic
# constraints/terminology promise `editor-legend-save` makes — a promise
# `HFS_VALIDATION_MODE` off (the default) would make false. The third line
# says where that attachment is actually edited, since this guided form
# never lists it (`hidden=["content"]`).
lib-form-legend-live = 輸入時即時檢查：結構、基數、必要繫結
lib-form-legend-save = 儲存時檢查：SQL on FHIR Library類型與SQL附件
lib-form-legend-content = SQL附件（content）不會列在此處：請在下方的SQL卡片中編輯
editor-deferred-badge = 儲存時
editor-deferred-hint = 儲存時會依值集驗證代碼（若已設定術語伺服器，選擇器中也會即時驗證）
editor-must-support-hint = Must-support：此Profile的使用方應能處理此元素
editor-add-filter = 篩選元素與擴充
editor-add-another = 再新增一個
editor-pick-type = 選擇類型…
editor-extension-url = 擴充URL
editor-add-extension = 新增擴充

editor-valid = 沒有問題。
editor-issues = { $count ->
    [one] { $count }個問題
   *[other] { $count }個問題
}

editor-modifier-badge = 修飾用
editor-modifier-warning = 修飾用擴充（modifier extension）會改變此資源的意義。無法識別它的系統必須拒絕處理此資源。
editor-unknown-badge = 不在綱要中
editor-unknown-hint = 綱要未描述此元素。顯示在此是為了避免它被無聲地遺失，儲存時會保留。

editor-primitive-extension-badge = + 擴充
editor-primitive-extension-hint = 此值本身帶有擴充（JSON中以 `_` 開頭的同層欄位）。儲存時會保留。

editor-collapse-all = 全部收合
editor-expand-all = 全部展開
json-view-toggle-fold = 切換JSON區段
editor-edit-raw = 編輯原始內容
editor-versions = 版本
editor-versions-none = 沒有先前的版本。

## Resources workspace (#282)

resources-heading = 資源
resources-lede = 瀏覽、搜尋、建立與編輯FHIR資源。可用自然語言搜尋或手動建構查詢，再開啟任一結果進行編輯。
resources-create = 建立新資源
resources-create-typed = 建立新的{ $type }
resources-create-invalid-type = 所選的FHIR版本中沒有此資源類型。請修正查詢，或從清單中選擇類型。
resources-create-not-advertised = 本伺服器不允許建立此資源類型。仍可搜尋它。
resources-create-schema-unavailable = 所選的FHIR版本中，此資源類型沒有編輯器綱要，因此UI無法安全地建立它。
resources-create-metadata-unavailable = 無法取得伺服器功能資訊。在UI能確認之前，建立功能維持停用。
resources-save-blocked = 請先修正驗證問題再儲存。
resources-save-invalid = JSON無效——請先修正再儲存。
resources-edit-title = 編輯資源
resources-tab-edit = 編輯
resources-tab-history = 歷程
resources-types-heading = 資源類型
rail-all-types-heading = 全部類型
# #1078: a type rail count from a snapshot counted from recent writes, not yet
# reconciled with storage. Hover title and screen-reader text of the "≈" count.
rail-count-approximate = 約略值：依最近的寫入計算，尚未與儲存層核對。

queries-saved-group = 已儲存

nav-collapse = 收合選單
nav-main = 主導覽
nav-menu-close = 關閉導覽選單
nav-menu-open = 開啟導覽選單

batch-heading = 批次／交易
batch-lede = 上傳FHIR Bundle，檢視它將執行的動作，對本伺服器執行，並查看每個項目的結果。
batch-upload = 上傳
batch-drop-hint = 將Bundle JSON檔案拖放到此處
batch-drop-browse = 或點擊以瀏覽檔案
batch-invalid-json = 該檔案不是有效的JSON。
batch-not-a-bundle = 該JSON不是FHIR Bundle。
batch-bad-type = 此處只能執行類型為batch或transaction的Bundle。
batch-request = 請求
batch-entries = 個項目
batch-semantics-batch = 批次（batch）：各項目獨立執行——某一項目失敗不會中止或復原其他項目。
batch-semantics-transaction = 交易（transaction）：全有或全無——任一項目失敗，伺服器就會回復整個Bundle。
batch-tab-actions = 動作
batch-tab-json = Bundle JSON
batch-no-body = （無內容——此項目僅指定資源位址）
batch-cancel = 取消
batch-execute = 執行
batch-plan-heading = 執行計畫
batch-done = 完成
batch-response-heading = 各動作結果
batch-sum-created = 已建立
batch-sum-updated = 已更新
batch-sum-other = 讀取／其他
batch-sum-failed = 失敗
batch-request-failed = 請求失敗。
batch-reading = 讀取Bundle中…
batch-executing = 執行中…
batch-read-failed = 無法讀取該檔案。

## Bulk Import workspace (#527)

bulk-import-title = 批次匯入
bulk-import-lede = 透過Bulk Data $bulk-submit操作，將預先整理的FHIR資料集送交資料接收方（Data Recipient）。
bulk-import-detail-lede = 此次提交的清單（manifest）、狀態與執行記錄。
bulk-import-new = 新增提交
bulk-import-create-title = 建立批次提交
bulk-import-field-name = 提交名稱
bulk-import-auth = 驗證方式
bulk-import-auth-none = 無
bulk-import-auth-none-hint = 不會送出authorization標頭。
bulk-import-auth-backend = 後端服務驗證（Backend services）
bulk-import-auth-backend-hint = 取得存取權杖後，以Bearer權杖的形式放在authorization標頭中送出。
bulk-import-field-client-id = Client ID
bulk-import-field-client-id-hint = 向資料接收方註冊本資料提供者，並取得client ID。
bulk-import-field-token-url = Token URL
bulk-import-field-token-url-hint = 授權伺服器的token端點URL。
bulk-import-jwks-hint = 請使用下列JWKS URL，向接收方註冊本伺服器的公開金鑰：
bulk-import-test-auth = 測試驗證
bulk-import-test-auth-ok = 驗證成功。
bulk-import-create-submit = 送出
bulk-import-advanced = 進階選項
bulk-import-unavailable = 儲存後端未提供設定儲存區，因此無法儲存提交。
bulk-import-submissions = 提交
bulk-import-records = 筆
bulk-import-col-name = 名稱
bulk-import-col-status = 狀態
bulk-import-col-created = 建立時間
bulk-import-col-destination = 目的地
bulk-import-empty = 尚無提交。建立一筆即可開始。
bulk-import-all = 所有提交
bulk-import-status-not-started = 尚未開始
bulk-import-status-in-progress = 進行中
bulk-import-status-stopped = 已停止
bulk-import-status-completed = 已完成
bulk-import-status-failed = 失敗
# Persistent banner on a submission whose last Abort / Mark completed never
# reached the Data Recipient (#968): the submission is still running and the
# button can simply be pressed again. { $detail } is the recipient's own
# untranslated diagnosis — an HTTP status and reason, or a transport error.
bulk-import-status-error = 上次的狀態變更未送達資料接收方，此提交維持不變——請再試一次。({ $detail })
# The same banner while the change is still queued (#998): the recipient
# never answered (as opposed to refusing), so the request is kept and re-sent
# from the status card's own refresh for as long as this page stays open.
bulk-import-status-pending = 上次的狀態變更未送達資料接收方；該請求已保留，只要此頁面保持開啟就會自動重送。({ $detail })
bulk-import-detail-recipient = 資料接收方
bulk-import-detail-id = 提交ID
bulk-import-detail-submitter = 提交者
bulk-import-detail-created = 建立時間
bulk-import-detail-status = 狀態
bulk-import-detail-auth = 驗證方式
bulk-import-abort = 中止
bulk-import-mark-completed = 標記為完成
bulk-import-delete = 刪除
bulk-import-edit = 編輯
bulk-import-edit-title = 編輯提交
bulk-import-edit-submit = 儲存變更
bulk-import-field-manifest-url = Manifest URL
bulk-import-field-manifest-url-hint = 指向Bulk Export Manifest的URL，其中包含預先整理的FHIR資料集。
bulk-import-field-output-format = 格式
bulk-import-field-output-format-hint = 清單中Bulk Data檔案的格式。
bulk-import-field-headers = 檔案請求標頭
bulk-import-field-headers-hint = 資料接收方請求資料檔案時應使用的HTTP標頭，每行一組「Name: value」。
bulk-import-log = 提交記錄
bulk-import-log-empty = 尚未送出任何內容。
bulk-import-field-submitter-system = 提交者system
bulk-import-field-submitter-value = 提交者value
bulk-import-field-submitter-hint = 必須與向資料接收方註冊的識別碼一致（需另行協調）。留空則使用自動產生的預設值。
bulk-import-processing = 處理中
bulk-import-processing-waiting = 正在等待接收方的第一份狀態回報…
bulk-import-result = 結果
bulk-import-result-finished = 處理完成時間：
bulk-import-result-outputs = 輸出檔案
bulk-import-result-errors = 錯誤檔案
ui-cancel = 取消
ui-close = 關閉
ui-combobox-selected-label = 已選取的項目
ui-combobox-remove = 移除
ui-combobox-added = 已加入
ui-combobox-removed = 已移除
ui-combobox-loading = 正在載入建議…
ui-combobox-results-updated = 可用的建議：
ui-combobox-error = 無法載入建議。請再試一次。
editor-orphans-title = 這些問題尚無對應欄位——請新增相關元素以修正
editor-hint-date = FHIR date：YYYY、YYYY-MM或YYYY-MM-DD
editor-hint-datetime = FHIR dateTime：YYYY、YYYY-MM、YYYY-MM-DD，或含時區的完整時間戳記（2024-05-17T14:30:00+02:00）
editor-hint-time = FHIR time：HH:MM:SS
editor-hint-instant = FHIR instant：含時區的完整時間戳記，例如2024-05-17T14:30:00.000Z

## Subscriptions operator page (#580)

subs-title = 訂閱
subs-lede = 訂閱引擎的唯讀檢視：所有已註冊的訂閱、其通道、即時狀態與傳送計數。
subs-unavailable = 此伺服器未啟用訂閱引擎。
subs-unavailable-how = 以下列方式啟動HFS即可開啟：
subs-empty = 此租戶尚未註冊任何訂閱。
subs-card-failing = 失敗中
subs-card-failing-sub = 需要處理
subs-card-idle = 閒置
subs-card-idle-sub = 沒有客戶端
subs-card-active = 有效
subs-card-active-sub = 傳送中
subs-card-delivered = 24小時內已傳送
subs-card-delivered-sub = { $rate }%首次即成功
subs-card-delivered-none = 此期間內沒有傳送
subs-table-heading = 訂閱
subs-sort = 排序
subs-sort-status = 狀態
subs-sort-sent = 傳送最多
subs-sort-fails = 連續失敗
subs-col-subscription = 訂閱
subs-col-channel = 通道
subs-col-status = 狀態
subs-col-last24 = 最近24小時
subs-col-sent = 已傳送
subs-col-fails = 連續失敗
subs-state-active = 有效
subs-state-error = 錯誤
subs-state-idle = 0個客戶端
subs-state-requested = 已請求
subs-state-off = 關閉

## Bulk Export workspace (#537)

bulk-export-title = 批次匯出
bulk-export-lede = 透過FHIR Bulk Data $export操作，將此伺服器的資料匯出為NDJSON檔案。
bulk-export-active-title = 匯出
bulk-export-new = 新增匯出
bulk-export-unavailable = 儲存後端未提供設定儲存區，因此無法追蹤匯出工作。
bulk-export-scope = 要匯出什麼？
bulk-export-scope-system = 全部
bulk-export-scope-system-hint = 整個伺服器——下方選取的每一種資源類型。
bulk-export-scope-patient = 病患
bulk-export-scope-patient-hint = 所有病患或僅限選取的病患，連同屬於他們的記錄。不含與病患無關的資料。
bulk-export-scope-group = 群組
bulk-export-scope-group-hint = 僅限已定義的世代（cohort）成員。
bulk-export-field-group-id = Group ID
bulk-export-field-group-id-hint = 群組範圍必填：要匯出的FHIR Group的id。
bulk-export-field-patients = 病患
bulk-export-field-patients-placeholder = 搜尋病患
bulk-export-field-patients-hint = 可依姓名、姓氏或精確識別碼搜尋。留空則匯出所有病患。
bulk-export-field-patients-fallback-placeholder = 病患FHIR ID
bulk-export-field-patients-fallback-hint = 輸入精確的FHIR邏輯ID，以逗號或換行分隔。留空則匯出所有病患。
bulk-export-field-patients-id-only-hint = 依精確的FHIR ID搜尋。留空則匯出所有病患。
bulk-export-patient-options-empty = 找不到符合的病患。
bulk-export-patient-invalid = 僅可輸入有效的Patient邏輯ID，以逗號或換行分隔。
bulk-export-field-name = 名稱
bulk-export-field-name-placeholder = 2024年糖尿病登錄
bulk-export-name-required = 請輸入此匯出的名稱。
bulk-export-types = 資源類型
bulk-export-all-resources = 所有資源
bulk-export-narrow = 縮小範圍
bulk-export-field-elements = FHIR元素
bulk-export-field-type-filter = 類型篩選
bulk-export-field-since = 起始時間
bulk-export-since-all = 全部時間
bulk-export-since-day = 最近1天
bulk-export-since-week = 最近7天
bulk-export-since-month = 最近4週
bulk-export-since-custom = 自訂
bulk-export-field-since-custom = 自訂instant
bulk-export-since-invalid = 請輸入有效的FHIR instant，例如2026-08-01T00:00:00Z。
bulk-export-field-until = 結束時間
bulk-export-field-until-hint = 選填的上限。RFC 3339格式，例如2026-08-01T00:00:00Z。
bulk-export-until-before-since = 結束時間不得早於起始時間。
bulk-export-window-since = 起始
bulk-export-window-until = 結束
bulk-export-start = 開始匯出
bulk-export-clear = 清除
bulk-export-files-count = { $count ->
    [one] { $count }個檔案
   *[other] { $count }個檔案
}
bulk-export-summary-count = { $count ->
    [one] { $count }筆匯出
   *[other] { $count }筆匯出
} · { $running }筆執行中
bulk-export-none = 尚無匯出。使用「新增匯出」來開始。
bulk-export-status-in-progress = 進行中
bulk-export-status-complete = 已完成
bulk-export-status-failed = 失敗
bulk-export-status-cancelled = 已取消
bulk-export-progress = 進度
bulk-export-progress-waiting = 正在等待第一份狀態回報…
bulk-export-writing = 正在寫入{ $name }
bulk-export-types-progress = { $done } / { $total }種類型
bulk-export-files = 檔案
bulk-export-finished-in = 耗時
bulk-export-error = 錯誤
bulk-export-cancel = 取消
bulk-export-cancel-refused = 伺服器拒絕取消：
bulk-export-retry = 重試
bulk-export-download-all = 下載所有資源
bulk-export-download-all-aria = 下載{ $name }的所有資源
bulk-export-delete = 刪除
bulk-export-delete-aria = 刪除匯出{ $name }
bulk-export-delete-warning = 確定要從伺服器刪除{ $name }及其輸出檔案嗎？此動作無法復原。
bulk-export-delete-confirm = 刪除匯出
bulk-export-delete-cancel = 保留匯出
bulk-export-delete-error = 無法安全地刪除此匯出。其卡片已保留，可再試一次。

# CapabilityStatement page (#653)
cap-title = 能力聲明（CapabilityStatement）
cap-lede = 此伺服器目前針對所選租戶與FHIR版本所支援的功能——即時由/metadata組成。
cap-summary-heading = 伺服器摘要
cap-summary-description = 說明
cap-summary-url = 基底URL
cap-summary-fhir-version = FHIR版本
cap-summary-status = 狀態
cap-summary-kind = 種類
cap-summary-date = 日期
cap-summary-formats = 格式
cap-interactions-heading = 系統層級互動
cap-transaction-note = 因為目前的後端支援原子交易，所以會宣告transaction；batch則一律可用。
cap-role-matrix = 檢視後端角色矩陣。
cap-operations-heading = 操作
cap-col-operation = 操作
cap-col-definition = 定義
cap-resources-heading = 各資源的功能
cap-filter-placeholder = 篩選類型…
cap-col-type = 類型
cap-col-interactions = 互動
cap-col-search-params = 搜尋參數
cap-col-includes = _include
cap-col-revincludes = _revinclude
cap-resources-empty = 沒有符合篩選條件的資源類型。
cap-raw-toggle = 原始CapabilityStatement (JSON)
cap-raw-load = 開啟純文字JSON
cap-raw-loading = 正在載入語法標示的JSON…
cap-raw-plain = 純文字JSON備援。JavaScript可用時會逐步載入語法標示。
cap-json-load-error = 無法載入此JSON區段。請收合後再展開以重試。
cap-json-truncated = 顯示已截斷
cap-json-path-too-deep = 已達最大展開深度
cap-json-pagination-label = JSON項目
cap-json-page-prev = 上一頁
cap-json-page-next = 下一頁
cap-json-collapse-all = 全部收合
cap-json-expand-all = 全部展開
cap-json-expanding = 正在展開可見的JSON…
cap-json-expand-complete = 所有JSON已展開。
cap-json-expand-partial = 已在安全上限內展開目前的頁面，其餘節點維持收合。
cap-json-limit-reached = 已達JSON顯示上限。請先收合其他分支再載入更多。
cap-json-expand-error = 無法展開JSON樹。已保留目前的檢視，請再試一次。
cap-unavailable = 無法從伺服器取得CapabilityStatement——啟用驗證時，自我呼叫可能需要對外權杖。

## Generic JSON fold labels (#898) — used by raw response folds in HTS-UI.
## The cap-* keys above are kept for CapabilityStatement specifically.
json-fold-loading = 正在載入JSON…
json-fold-load-error = 無法載入此JSON區段。請收合後再展開以重試。
json-fold-open-plain = 開啟純文字JSON
json-fold-plain = 純文字JSON備援。JavaScript可用時會逐步載入語法標示。

## SQL on FHIR section stubs (#649)


sql-vd-title = ViewDefinition
sql-vd-lede = 撰寫並管理SQL on FHIR執行時用來攤平資源的ViewDefinition。

sql-queries-title = SQL查詢
sql-queries-lede = 對ViewDefinition產出的平面資料表執行臨時SQL。結果會隨輸入即時更新。

sql-views-title = SQL檢視表
sql-views-lede = 供其他查詢與匯出選取的具名檢視表。只需定義一次SQL；其產出的欄位就會成為所有人都能使用的資料表。

sql-export-title = SQL匯出
sql-export-lede = 將已儲存的ViewDefinition、SQL查詢與SQL檢視表作為長時間執行的$sql-export工作執行，並下載輸出檔案。
sql-export-active-title = SQL匯出

## View Definitions workspace (#649)

vd-new = 建立新項目
vd-new-title = 新增ViewDefinition
vd-rail-label = ViewDefinition清單
vd-rail-heading = ViewDefinition
vd-rail-all-heading = 所有ViewDefinition
vd-filter = 篩選檢視表
vd-none = 尚無ViewDefinition。
vd-empty-lede = 使用「建立新項目」建立第一個ViewDefinition。
vd-degraded = 無法載入ViewDefinition清單。
vd-saved = 已儲存。
vd-run-failed = 無法執行此檢視表。
vd-save = 儲存
vd-duplicate = 複製
sql-duplicate-read-failed = 複製前無法檢查現有的成品：{ $reason }
sql-duplicate-incomplete = 無法檢查所有現有的成品。副本未儲存；請待目錄可完整讀取後再試。
vd-delete = 刪除
vd-delete-confirm = 確定要刪除ViewDefinition「{ $name }」嗎？此操作無法復原。
vd-delete-failed = 無法刪除此ViewDefinition。
vd-json-heading = 定義(JSON)
vd-run-hint = 輸入時即時執行 — 結果依據目前的定義，無論是否已儲存
vd-results-heading = 結果
vd-results-empty = 此檢視表未產生任何資料列。
# #752: the $sql-run preview's row/duration meta, shared by the page's own
# render and the `/run` fragment endpoint.
vd-results-meta = { $rows }列 · { $ms } ms
# #752: the results meta after a failed run — the previous table stays on
# screen, relabelled.
vd-results-stale = 上次成功執行的結果
vd-pagination-label = ViewDefinition分頁
vd-page-prev = 上一頁
vd-page-next = 下一頁

## ViewDefinition lint messages and fixes (#821): the `/lint` handler's own
## localization of `helios_sof::lint::Diagnostic` — code + args here, never
## `Diagnostic.message` (English-only, unlocalized) itself. `vd-lint-*` keys
## are named after `DiagnosticCode`'s own kebab-case wire form; a
## `missing-required`/`wrong-type` diagnostic from a constant's value[x]
## choice carries `$variant` ("missing"/"multiple") instead of the generic
## `$key`/`$expected`+`$found` args, selected on below. `vd-fix-*` keys label
## one `Fix` each, shown next to the diagnostic as a one-click action.
vd-lint-not-a-view-definition = 不是ViewDefinition：實際為{ $found }
vd-lint-unknown-key = 未知的鍵「{ $key }」
vd-lint-missing-required = { $variant ->
    [missing] constant必須設定且僅設定一個值
   *[other]缺少必要的鍵「{ $key }」
}
vd-lint-wrong-type = { $variant ->
    [multiple] constant只能設定一個值
   *[other]預期為{ $expected }，實際為{ $found }
}
vd-lint-empty-required = 必要的鍵「{ $key }」不得為空
vd-lint-duplicate-column-name = 重複的column名稱「{ $name }」
vd-lint-multiple-iteration-directives = 一個select最多只能設定forEach、forEachOrNull、repeat其中之一（實際為{ $keys }）
vd-lint-select-without-output = 一個select至少必須包含column、select或unionAll其中之一
vd-lint-fhirpath-syntax = FHIRPath語法錯誤：{ $detail }
vd-lint-undeclared-constant = 未宣告的constant「%{ $name }」
vd-lint-unknown-resource-type = 未知的資源類型「{ $found }」
vd-fix-rename-key = 重新命名為「{ $to }」
vd-fix-remove-key = 移除「{ $key }」
vd-fix-set-string = 設為「{ $value }」

## ViewDefinition completion (#821): `vd-editor.js` reads this once off
## `#vd-editor-grid`'s own `data-msg-required` — the marker a required
## structural key's completion item carries, appended to its `detail` text
## client-side (never sent by `/complete` itself, which is not locale-aware).
vd-complete-required = 必填

## ViewDefinition editor save-with-errors confirmation (#821): `vd-editor.js`
## picks whichever of these renders `#vd-editor-grid`'s own
## `data-msg-save-errors-one`/`-other` and substitutes the real count for the
## literal `{count}` marker below, itself the value `$count` was given —
## `window.confirm`'s own text has no locale of its own, so this is the only
## way to translate it. Shown only when submitting as Save (not Duplicate)
## while the most recently completed lint pass still has at least one error.
vd-save-with-errors-one = 此ViewDefinition仍有{ $count }個錯誤。仍要儲存嗎？
vd-save-with-errors-other = 此ViewDefinition仍有{ $count }個錯誤。仍要儲存嗎？

## $sql-run results partial (#752): shared by the View Definitions, SQL
## Queries, and SQL Views playgrounds (#839).

sql-run-export = 匯出為檔案

## SQL Queries / SQL Views workspaces (#649, editor-first layout #839)

sql-queries-new-title = 新增SQL查詢
sql-views-new-title = 新增SQL檢視表
sql-queries-rail-all-heading = 所有SQL查詢
sql-views-rail-all-heading = 所有SQL檢視表
# The title row's `.tag--type` chip (#839) — one document's type, singular,
# distinct from the *-title keys above (the collection's own plural name).
sql-queries-chip = SQL查詢
sql-views-chip = SQL檢視表
sql-queries-filter = 篩選查詢
sql-views-filter = 篩選檢視表
sql-queries-rail-empty = 尚無查詢。
sql-views-rail-empty = 尚無檢視表。
sql-queries-empty-title = 尚無SQL查詢
sql-views-empty-title = 尚無SQL檢視表
sql-queries-empty-lede = 使用「建立新項目」撰寫第一個查詢。它會對所有有效ViewDefinition的平面資料表執行。
sql-views-empty-lede = 使用「建立新項目」定義第一個檢視表。其欄位會成為其他查詢與匯出可選取的資料表。
sql-queries-editor-heading = SQL
sql-views-editor-heading = 檢視表定義(SQL)
sql-queries-results-heading = 結果
sql-views-results-heading = 預覽
sql-queries-run-failed = 無法執行此查詢。
sql-views-run-failed = 無法執行此檢視表。
lib-degraded = 無法載入Library清單。
# The SQL card's "runs as you type" legend (#839) — shared verbatim by SQL
# Queries and SQL Views, unlike the headings/failure prefix above.
lib-run-hint = 輸入時即時執行 — 結果依據目前的SQL，無論是否已儲存
# The SQL card's own notice (#1233), shown next to the legend above only
# while the Details JSON's `application/sql` attachment does not decode
# (invalid base64, non-UTF-8 bytes, or a missing `data`) — the card keeps
# showing its last readable text rather than clearing it, and typing there
# repairs the attachment. `sql-library-sync.js` is the only thing that ever
# shows it; the server always paints it hidden.
lib-sql-attachment-unreadable = SQL附件無法讀取：SQL卡片保留最後可讀取的內容；在此輸入即可修復
lib-delete-confirm = 確定要刪除「{ $name }」嗎？此操作無法復原。
lib-delete-failed = 無法刪除此Library。
# Details section (#840): the full stored Library (SQL attachment
# included, #1233), edited as JSON (left) and through the guided form
# (right).
lib-details-heading = 詳細資料
lib-details-json-heading = Library (JSON)
lib-details-json-note = SQL附件(content[].data)是此文件的一部分。下方的SQL卡片編輯的是同一個附件。
# Shown only for `?lib=new`, under the Details heading — closes the #839
# follow-up asking for a hint about the starter's `change-me` placeholder.
lib-details-new-lede = 請重新命名，並將relatedArtifact[0]指向一個實際存在的ViewDefinition。
# The Save gate (#840): rejects a document whose `type.coding` names the
# other kind's code (a sql-view saved from SQL Queries, or the reverse),
# which would otherwise silently vanish it from the rail it was just edited
# on. `$code` is the route's own expected code ("sql-query"/"sql-view").
lib-save-wrong-kind = 此Library的SQL on FHIR類型必須是「{ $code }」才能在此儲存。
# The Save gate's SQL View counterpart (#841): the SQLView profile
# fixes `Library.parameter` to 0..0, so a non-empty declaration is rejected
# the same way the wrong `type.coding` above is. The identical text also
# backs the `/run` fragment's own pre-`$sql-run` check for the same case —
# one message, two call sites.
lib-save-view-parameters = SQL檢視表不能宣告參數（SQLView profile，Library.parameter 0..0）。請在「詳細資料」中移除parameter[]。

## Parameters card (#841, SQL Query only)

lib-params-heading = 參數
lib-params-meta = 宣告於Library · 下方的值僅用於本次執行
lib-params-empty = 尚未宣告任何參數。可在此處或「詳細資料」中新增。
# `$name` is the bare placeholder name, without its leading `:` — both
# messages below add it themselves so a translation can place it anywhere
# in the sentence.
lib-params-hint = SQL中使用了:{ $name }，但Library上未宣告。
lib-params-declare = 宣告:{ $name }
lib-params-add-toggle = 新增參數
lib-params-add-name-label = 名稱
lib-params-add-type-label = 類型
lib-params-add-submit = 新增
# `add-parameter`'s own validation messages (#841) — type codes
# (`string`, `integer`, …) are never translated.
lib-params-add-invalid-name = 參數名稱必須符合 ^[A-Za-z][A-Za-z0-9_]*$
lib-params-add-duplicate = 已宣告名為{ $name }的參數
lib-params-add-unknown-type = 未知的參數類型
# The live-run "waiting for a value" notice (#841): `$names` is
# already comma-joined with each name's own leading `:` by the caller (one
# name, or several), since a Fluent list can't format an arbitrary-length
# collection on its own.
lib-run-waiting = 正在等待{ $names }的值 — 下方結果來自上次成功的執行。

# The unknown-table lint's own live-run notice (#842/04): the SQL reads a
# table no declared `relatedArtifact[depends-on]` label names, so `$sql-run`
# is never called. `lib-run-unknown-table` is the *first* unknown table's
# own full sentence (the notice's own banner text when there is only one);
# `lib-run-unknown-table-more` is the short form every additional table
# (and every table's own hover tooltip in the editor) uses instead. `$line`
# is the table's own 1-based line number.
lib-run-unknown-table = 未知的資料表{ $name } — 第{ $line }行。請在「讀取來源」中宣告或修正名稱。SQL未變更；下方結果來自上次成功的執行。
lib-run-unknown-table-more = 未知的資料表{ $name } — 第{ $line }行。

## Tables panel (#842, both kinds) — Reads from / Used by / Columns

lib-tables-heading = 讀取來源
# The `<code>` cell's own placeholder for a `relatedArtifact[depends-on]`
# entry with no `label` at all — a malformed document, most commonly one
# hand-edited in Details.
lib-tables-no-alias = (無標籤)
lib-tables-empty = 尚未宣告任何資料表。
# The resolved-target chip text. "SQL View" reuses `sql-views-chip` — the
# same chip the title row and *Used by* already show for that kind.
lib-tables-kind-view-definition = ViewDefinition
lib-tables-target-not-found = 找不到
lib-tables-target-not-found-detail = 沒有任何ViewDefinition或SQL檢視表對應到{ $resource }。請在「詳細資料」中修正canonical URL或移除此列。
lib-tables-target-not-a-table = 不是資料表
lib-tables-target-not-a-table-detail = 只能讀取ViewDefinition或SQL檢視表。
lib-tables-add-table-label = 資料表
# The always-visible add row's own search field placeholder (#1238).
lib-tables-add-table-placeholder = 新增資料表…
lib-tables-add-table-fallback-placeholder = ViewDefinition/{"{"}id{"}"}或Library/{"{"}id{"}"}
lib-tables-alias-label = 別名
lib-tables-add-submit = 新增
# The add row's own single help line (#1238) — the only hint the row
# carries; the combobox's own `hint`/`fallback-hint` are passed empty.
lib-tables-add-hint = 搜尋ViewDefinition或SQL檢視表；別名預設為其名稱。
# `add-table`'s own validation messages (#842), in the order they are
# checked.
lib-tables-add-error-required = 請選擇一個ViewDefinition或SQL檢視表
lib-tables-add-error-alias-required = 別名為必填
lib-tables-add-error-alias-invalid = 別名必須符合 ^[A-Za-z][A-Za-z0-9_]*$
lib-tables-add-error-alias-duplicate = 別名{ $alias }已宣告
# The *Add table* combobox's own "no matches" result message
# (`partials/lookup_options.html`), mirroring `bulk-export-patient-options-
# empty`/`sql-export-group-options-empty` for the two existing pickers.
lib-tables-options-empty = 沒有符合的項目。
# The unknown-table lint's own row (#842/04): a table the SQL reads that no
# dependency declares, appended after every resolved row.
lib-tables-unknown = 未知的資料表
lib-tables-unknown-detail = SQL中有使用但未宣告。請選擇一個ViewDefinition或SQL檢視表，或修正名稱。
lib-tables-declare = 宣告{ $name }
# The row's own visible *Declare* button text (#1238) — the accessible
# name carries the table's own name instead (`lib-tables-declare`).
lib-tables-declare-short = 宣告
# The row's own icon *Remove* button's accessible name (#1238) — `$alias`
# is the alias exactly as the `<code>` cell shows it, including the
# `lib-tables-no-alias` placeholder when the row has none.
lib-tables-remove-row = 移除{ $alias }

lib-used-by-heading = 被引用於
lib-used-by-empty = 尚無任何項目使用它。
lib-used-by-export-kind = SQL匯出

lib-columns-heading = 欄
lib-columns-meta-query = 查詢的產出欄位
lib-columns-meta-view = 檢視表的產出欄位
lib-columns-empty-query = 執行查詢以查看其欄位。
lib-columns-empty-view = 執行檢視表以查看其欄位。
lib-columns-col-name = 欄
lib-columns-col-type = 類型
lib-columns-col-from = 來源

## SQL Export pages (#649, #833)

export-start-failed = 無法啟動匯出。
export-format = 輸出格式
export-start = 開始匯出

## SQL Export builder — create form (#834)

sql-export-name-placeholder = 每月病人平面檔
sql-export-subjects-legend = 要匯出什麼？
sql-export-subjects-heading = 匯出項目
sql-export-filter-all = 全部
sql-export-filter-view-definitions = ViewDefinition
sql-export-filter-queries = 查詢
sql-export-filter-views = 檢視表
sql-export-filter-subjects = 篩選匯出項目
sql-export-select-all = 全選
sql-export-col-subject = 項目
sql-export-col-kind = 種類
sql-export-col-status = 狀態
sql-export-selected-count = 已選取{ $selected }／{ $total }個
sql-export-format-ndjson-hint = 每列一個JSON物件，適合串流進資料管線。
sql-export-format-csv-hint = 標題列加上每列一行，任何試算表都能開啟。
sql-export-format-json-hint = 每個輸出為單一JSON陣列，最容易人工檢視。
sql-export-format-parquet-hint = 欄式且具型別，最適合分析引擎。
sql-export-empty-heading = 目前沒有可匯出的項目
sql-export-empty-body = 所有已儲存的ViewDefinition、SQL查詢與SQL檢視表都會在此列為匯出項目。請先建立一個。
sql-export-filter-empty = 沒有符合的項目

## SQL Export builder — "Narrow it down" and "Advanced" (#836)

sql-export-narrow-title = 縮小範圍
sql-export-narrow-meta = 選填 — 留空則匯出全部
sql-export-field-patients = 病人
sql-export-field-patients-placeholder = 搜尋病人
sql-export-field-patients-hint = 依名字、姓氏或精確識別碼搜尋。留空則匯出所有病人。
sql-export-field-patients-fallback-placeholder = Patient FHIR ID
sql-export-field-patients-fallback-hint = 輸入精確的FHIR邏輯ID，以逗號或換行分隔。留空則匯出所有病人。
sql-export-field-patients-id-only-hint = 依精確的FHIR ID搜尋。留空則匯出所有病人。
sql-export-field-groups = 群組
sql-export-field-groups-placeholder = 搜尋群組
sql-export-field-groups-hint-r4 = 依精確的FHIR ID或識別碼搜尋。
sql-export-field-groups-hint-r5 = 依名稱、精確的FHIR ID或識別碼搜尋。
sql-export-field-groups-fallback-placeholder = Group FHIR ID
sql-export-field-groups-fallback-hint = 輸入精確的FHIR ID或識別碼，以逗號或換行分隔。
sql-export-group-options-empty = 找不到符合的群組。
sql-export-field-since = 起始時間
sql-export-since-all = 全部時間
sql-export-since-day = 最近1天
sql-export-since-week = 最近7天
sql-export-since-month = 最近4週
sql-export-since-custom = 自訂
sql-export-field-since-custom = 自訂時間點
sql-export-since-invalid = 請輸入有效的FHIR instant，例如2026-08-01T00:00:00Z。
sql-export-patient-invalid = 只能輸入有效的Patient邏輯ID，以逗號或換行分隔。
sql-export-group-invalid = 只能輸入有效的Group邏輯ID，以逗號或換行分隔。
sql-export-advanced = 進階
sql-export-advanced-meta = 追蹤ID · CSV標題列
sql-export-field-tracking-id = 追蹤ID
sql-export-field-tracking-id-hint = 會以clientTrackingId回傳在完成清單（manifest）中。
sql-export-tracking-id-too-long = 追蹤ID不得超過200個字元。
sql-export-field-header = 包含標題列
sql-export-field-header-hint = （僅限CSV — 其他格式會忽略）

## Active SQL Exports list and job cards (#833)

sql-export-new = 新增SQL匯出
sql-export-unavailable = 儲存後端未提供設定儲存區，因此無法追蹤SQL匯出工作。
sql-export-none = 尚無SQL匯出。請使用「新增SQL匯出」開始。
sql-export-store-error = 匯出已啟動，但無法加入此清單。工作ID：
sql-export-summary-count = { $count ->
    [one] { $count }個匯出
   *[other] { $count }個匯出
} · { $running }個執行中
sql-export-select-subject = 請至少選取一個匯出項目。
sql-export-unknown-subject = 一或多個選取的項目已不存在。請重新整理頁面後再試。
sql-export-cancelled-reason = 伺服器已不認得此工作
sql-export-status-in-progress = 進行中
sql-export-status-complete = 已完成
sql-export-status-failed = 失敗
sql-export-status-cancelled = 已取消
sql-export-view-files = 檢視檔案
sql-export-cancel = 取消
sql-export-retry = 重試
sql-export-run-again = 再次執行
sql-export-remove = 從清單移除
sql-export-more-actions = 更多動作
sql-export-copy-job-id = 複製工作ID
sql-export-copied = 已複製
sql-export-progress-waiting = 等待第一次狀態回報…
sql-export-writing = 正在寫入{ $name }
sql-export-subjects-progress = { $total ->
    [one]已完成{ $done }／{ $total }個項目
   *[other]已完成{ $done }／{ $total }個項目
}
sql-export-started = 開始於
sql-export-finished-in = 耗時
sql-export-cancelled-at = 取消於
sql-export-status-unavailable = 狀態無法取得
sql-export-format-ndjson = NDJSON
sql-export-format-csv = CSV
sql-export-format-json = JSON
sql-export-format-parquet = Parquet
sql-export-subjects-count = { $count ->
    [one] { $count }個項目
   *[other] { $count }個項目
}
sql-export-kind-view-definition = { $count ->
    [one] { $count }個ViewDefinition
   *[other] { $count }個ViewDefinition
}
sql-export-kind-sql-query = { $count ->
    [one] { $count }個SQL查詢
   *[other] { $count }個SQL查詢
}
sql-export-kind-sql-view = { $count ->
    [one] { $count }個SQL檢視表
   *[other] { $count }個SQL檢視表
}
sql-export-files-count = { $count ->
    [one] { $count }個檔案
   *[other] { $count }個檔案
}

## Job detail page (#835)

sql-export-detail-finished = 完成於
sql-export-detail-failed = 失敗於
sql-export-detail-cancelled = 取消於
sql-export-detail-started = 開始於
sql-export-detail-after = 耗時
sql-export-failure-subject = 匯出停止於項目
sql-export-failure-generic = 匯出失敗：
sql-export-detail-job-heading = 工作
sql-export-detail-field-job-id = 工作ID
sql-export-detail-field-format = 格式
sql-export-detail-field-started = 開始時間
sql-export-detail-field-duration = 耗時
sql-export-detail-field-subjects = 匯出項目
sql-export-detail-header-included = 含標題列
sql-export-detail-header-omitted = 不含標題列
sql-export-detail-outputs-heading = 輸出檔案
sql-export-detail-col-output = 輸出
sql-export-detail-col-subject = 項目
sql-export-detail-col-files = 檔案
sql-export-detail-outputs-empty = 此工作未產生任何輸出檔案。
sql-export-file-fallback = 檔案{ $n }

## #837: per-SQL-Query parameter values on the SQL Export builder
sql-export-param-count = { $count ->
    [one] { $count }個參數
   *[other] { $count }個參數
}
sql-export-param-required = 此值為必填。
sql-export-param-type-mismatch = 需要{ $type }類型的值。
sql-export-param-required-chip = 必填
sql-export-toggle-values = 顯示或隱藏值
sql-export-values-missing-one = 缺少{ $count }個值
sql-export-values-missing-other = 缺少{ $count }個值

## partials/sql_parameter_fields.html (#837) — shared with the SQL Query
## page's own parameter form
sql-param-default = 預設值

## HTS administrative UI (crates/hts-ui) — Phase 1 scaffold stubs
##
## Keys for the HTS UI follow the convention hts-<page>-<role>-<control>. These stubs cover the base layout, sidebar
## nav, and the dashboard scaffold placeholder rendered by the Phase 1 blocker
## slice. They must be kept in parity with es/de/main.ftl.

-hts-app-name = Helios Terminology Server
hts-app-title = { -hts-app-name }

hts-nav-section-work = 術語
hts-nav-section-tools = 工具
hts-nav-section-server = 伺服器
hts-nav-home = 首頁
hts-nav-code-systems = 代碼系統
hts-nav-value-sets = 值集
hts-nav-concept-maps = 概念對應
hts-nav-operations = 操作
hts-nav-import = 匯入

hts-fhir-version-heading = FHIR版本
hts-fhir-version = FHIR { $version }

hts-home-title = 首頁
hts-home-subtitle = 術語伺服器健康狀態、目錄內容與快速操作。

## Dashboard rows (row headings are visually hidden — they're for screen readers).

hts-home-row-status = 伺服器狀態

## Dashboard tiles.

hts-home-tile-status = 狀態
hts-home-tile-uptime = 運作時間
hts-home-tile-loaded-systems = 已載入的代碼系統
hts-home-tile-loaded-systems-hint = 來自TerminologyCapabilities.codeSystem[]
hts-home-tile-requests = 請求數
hts-home-tile-metrics-hint = 自伺服器啟動起

## Home request-rate chart (design doc §7.1). Plots a rate differenced from
## the cumulative `/metrics` counters, sampled only while this page is open,
## so several honest "nothing to draw" states need their own copy.

hts-home-chart-title = 每分鐘請求數
hts-home-chart-window = 圖表時間範圍
hts-home-chart-series = 狀態碼類別
hts-home-chart-window-15m = 15分
hts-home-chart-window-1h = 1小時
hts-home-chart-window-6h = 6小時
hts-home-chart-series-all = 全部
hts-home-chart-series-2xx = 2xx
hts-home-chart-series-4xx = 4xx
hts-home-chart-series-5xx = 5xx
hts-home-chart-empty-unreachable = 無法連線至/metrics — 沒有新的取樣資料進來。
hts-home-chart-empty-none = 尚未收集任何取樣資料。
hts-home-chart-empty-first = 正在收集第一個區間 — 計算速率需要兩個取樣點。
hts-home-chart-empty-window = 此時間範圍內沒有取樣資料。只有在此頁面開啟時才會取樣。
hts-home-chart-axis-now = 現在
hts-home-chart-axis-minutes = -{ $n }分
hts-home-chart-axis-hours = -{ $n }小時

## /health `status` values, keyed for translation.

hts-home-status-ok = 正常

## Degraded banner (design doc §7 header contract).

hts-degraded-title = 術語後端未完全可用
hts-degraded-body = 在HTS恢復連線前，部分區塊會先隱藏；受影響頁面上的互動控制項已停用。
hts-degraded-reason-client-build = 無法建立上游HTTP用戶端。
hts-degraded-reason-upstream-down = 無法連線至術語伺服器。
hts-degraded-reason-upstream-timeout = 術語伺服器未在時限內回應。
hts-degraded-reason-upstream-error = 術語伺服器回傳了錯誤狀態。
hts-degraded-reason-upstream-shape = 術語伺服器回傳了非預期的回應結構。
hts-degraded-reason-bootstrapping = 術語伺服器仍在載入初始資料。
hts-degraded-reason-unknown = 術語伺服器暫時無法使用。

## Dialect chip (topbar, session-wide displayLanguage / Accept-Language per §7.1).


## OperationOutcome partial (shared, design doc §7 / §11).

hts-outcome-severity = 嚴重程度：{ $severity }
hts-outcome-request-id = 請求ID：{ $id }
hts-outcome-code-not-found = 找不到所要求的資源。
hts-outcome-code-invalid = 請求因無效而被拒絕。
hts-outcome-code-too-costly = 所要求的操作因成本過高而被拒絕。
hts-outcome-code-unknown = 伺服器回傳了UI無法辨識的問題。
hts-degraded-since = 自{ $timestamp }起

## HTS Slice B — CodeSystem browser + detail with embedded workbench
## (design doc §7.2 + §7.3). Every key here has a peer in es/de/main.ftl.

## CodeSystem status pills (used by browser rows and detail header).

hts-cs-status-draft = 草稿
hts-cs-status-active = 有效
hts-cs-status-retired = 已停用
hts-cs-status-unknown = 未知

## CS browser page.

hts-cs-browser-title = CodeSystem
hts-cs-browser-subtitle = 瀏覽術語伺服器的CodeSystem目錄，開啟任一列即可檢視其中繼資料與工作台。
hts-cs-browser-filter-legend = 篩選CodeSystem
hts-cs-browser-filter-url = 正準URL
hts-cs-browser-filter-version = 版本
hts-cs-browser-filter-name = 名稱
hts-cs-browser-filter-title = 標題
hts-cs-browser-filter-status = 狀態
hts-cs-browser-filter-reset = 重設
hts-cs-browser-empty = 沒有符合這些篩選條件的CodeSystem。
hts-cs-browser-load-more = 載入更多
hts-cs-browser-showing-count = 顯示{ $count ->
    [one] { $count }個CodeSystem
   *[other] { $count }個CodeSystem
}
hts-cs-browser-table-caption = 符合目前篩選條件的CodeSystem。
hts-cs-browser-column-url = URL
hts-cs-browser-column-version = 版本
hts-cs-browser-column-title = 標題
hts-cs-browser-column-status = 狀態
hts-cs-browser-column-name = 名稱

## Phase 5 — HTS search-form shared strings (used by CS / VS / CM browsers).

hts-search-rail-label = 搜尋篩選條件
hts-search-rail-heading = 篩選條件
hts-facet-status-any = 任何狀態

## CS detail page.

hts-cs-detail-title = { $name } · CodeSystem
hts-cs-detail-title-fallback = CodeSystem
hts-cs-detail-eyebrow = CodeSystem
hts-cs-detail-section-identity = 識別資訊
hts-cs-detail-section-content = 內容
hts-cs-detail-content-mode = 內容模式
hts-cs-detail-count = 概念數量
hts-cs-detail-publisher = 發行者
hts-cs-detail-jurisdiction = 適用地區
hts-cs-detail-supersedes = 取代
hts-cs-detail-superseded-by = 被取代於
hts-cs-detail-tabs-label = CodeSystem工作台區段
hts-cs-detail-tab-lookup = 查詢
hts-cs-detail-tab-validate = 驗證
hts-cs-detail-tab-subsumes = 包含關係
hts-cs-detail-result-empty = 執行操作後，結果會顯示在這裡。

## CS $lookup form + result labels.

hts-cs-lookup-heading = 查詢概念
hts-cs-lookup-code = 代碼
hts-cs-lookup-version = 版本
hts-cs-lookup-display-language = 顯示語言
hts-cs-lookup-display-language-placeholder = 例如en-GB
hts-cs-lookup-properties-legend = 屬性
hts-cs-lookup-designations = 名稱設計（designation）
hts-cs-lookup-properties = 屬性
hts-cs-lookup-no-match = HTS未回傳符合的概念。

## CS $validate-code form + result labels.

hts-cs-validate-heading = 驗證代碼
hts-cs-validate-mode-legend = 輸入模式
hts-cs-validate-mode-code = 單一代碼
hts-cs-validate-mode-coding = Coding
hts-cs-validate-code-legend = 單一代碼
hts-cs-validate-code = 代碼
hts-cs-validate-display = 顯示名稱
hts-cs-validate-coding-legend = Coding
hts-cs-validate-coding-system = system
hts-cs-validate-coding-code = code
hts-cs-validate-coding-display = display
hts-cs-validate-badge-true = 有效
hts-cs-validate-badge-false = 無效
hts-cs-validate-message = 訊息

## CS $subsumes form + result labels.

hts-cs-subsumes-heading = 測試包含關係
hts-cs-subsumes-scoped-system = 代碼系統（固定）
hts-cs-subsumes-code-a = 代碼A
hts-cs-subsumes-code-b = 代碼B
hts-cs-subsumes-outcome-equivalent = 兩個代碼等價。
hts-cs-subsumes-outcome-subsumes = 代碼A包含代碼B。
hts-cs-subsumes-outcome-subsumed-by = 代碼A被代碼B包含。
hts-cs-subsumes-outcome-not-subsumed = 兩個代碼互不包含。

## Shared workbench chrome (reused by Slice C/D/E workbenches).

hts-workbench-run = 執行
hts-workbench-raw-response = 原始請求與回應
hts-workbench-copy-url = 請求URL
hts-workbench-request-body = 請求內容
hts-workbench-response-body = 回應內容

## Additional degraded reason for CS-read 404s (design doc §7.3 states matrix).

hts-degraded-reason-upstream-not-found = 術語伺服器找不到該資源。

## HTS Slice C — ValueSet browser + detail with embedded $expand workbench
## (design doc §7.4 + §7.4.1). Every key here has a peer in es/de/main.ftl.

## ValueSet status pills.

hts-vs-status-draft = 草稿
hts-vs-status-active = 有效
hts-vs-status-retired = 已停用
hts-vs-status-unknown = 未知

## VS browser page.

hts-vs-browser-title = ValueSet
hts-vs-browser-subtitle = 瀏覽術語伺服器的ValueSet目錄，開啟任一列即可檢視其中繼資料或執行展開。
hts-vs-browser-filter-legend = 篩選ValueSet
hts-vs-browser-filter-url = 正準URL
hts-vs-browser-filter-version = 版本
hts-vs-browser-filter-name = 名稱
hts-vs-browser-filter-title = 標題
hts-vs-browser-filter-status = 狀態
hts-vs-browser-filter-reset = 重設
hts-vs-browser-empty = 沒有符合這些篩選條件的ValueSet。
hts-vs-browser-load-more = 載入更多
hts-vs-expand-count-hint = 每頁最多 500 個概念；其餘請使用位移或「載入更多」。
hts-browser-goto = 前往
hts-browser-goto-offset = 跳至位移
hts-browser-page-next = 下一頁
hts-browser-page-prev = 上一頁
hts-browser-pager = 分頁
hts-browser-of-total = ，共 { $total } 筆
hts-vs-browser-showing-count = 顯示{ $count ->
    [one] { $count }個ValueSet
   *[other] { $count }個ValueSet
}
hts-vs-browser-table-caption = 符合目前篩選條件的ValueSet。
hts-vs-browser-column-url = URL
hts-vs-browser-column-version = 版本
hts-vs-browser-column-title = 標題
hts-vs-browser-column-status = 狀態
hts-vs-browser-column-name = 名稱

## VS detail page.

hts-vs-detail-title = { $name } · ValueSet
hts-vs-detail-title-fallback = ValueSet
hts-vs-detail-eyebrow = ValueSet
hts-vs-detail-section-identity = 識別資訊
hts-vs-detail-section-governance = 治理資訊
hts-vs-detail-publisher = 發行者
hts-vs-detail-jurisdiction = 適用地區
hts-vs-detail-immutable = 不可變更
hts-vs-detail-immutable-yes = 是
hts-vs-detail-immutable-no = 否
hts-vs-detail-purpose = 用途
hts-vs-detail-copyright = 版權
hts-vs-detail-tabs-label = ValueSet工作台區段
hts-vs-detail-tab-expand = 展開
hts-vs-detail-result-empty = 執行操作後，結果會顯示在這裡。

## VS $expand form + result labels.

hts-vs-expand-heading = 展開此ValueSet
hts-vs-expand-scoped-valueset = ValueSet（固定）
hts-vs-expand-filter = 篩選
hts-vs-expand-filter-placeholder = 代碼或顯示名稱文字
hts-vs-expand-count = count
hts-vs-expand-offset = offset
hts-vs-expand-display-language = 顯示語言
hts-vs-expand-display-language-placeholder = 例如en-GB
hts-vs-expand-flags-legend = 選項
hts-vs-expand-active-only = 僅限有效概念
hts-vs-expand-include-designations = 包含名稱設計（designation）
hts-vs-expand-mode-legend = 結果模式
hts-vs-expand-mode-flat = 平面
hts-vs-expand-mode-tree = 樹狀
hts-vs-expand-use-supplement-legend = 使用補充資源
hts-vs-expand-use-supplement-placeholder = 正準URL
hts-vs-expand-advanced-summary = 進階
hts-vs-expand-date = 日期
hts-vs-expand-date-placeholder = ISO 8601（例如2025-06-01）
hts-vs-expand-property-legend = 屬性
hts-vs-expand-property-placeholder = 屬性代碼
hts-vs-expand-tx-resource-legend = tx-resource
hts-vs-expand-tx-resource-placeholder = 正準URL或參照
hts-vs-expand-system-version-legend = system-version
hts-vs-expand-system-version-placeholder = system|version
hts-vs-expand-check-system-version-legend = check-system-version
hts-vs-expand-force-system-version-legend = force-system-version
hts-vs-expand-default-valueset-version = default-valueset-version
hts-vs-expand-threshold = 過於耗費（too-costly）門檻
hts-vs-expand-ceiling-tooltip = UI上限：{ $ceiling }（超過的值會被捨棄）
hts-vs-expand-ceiling-note = 上限：{ $ceiling }
hts-vs-expand-ceiling-warning-title = 門檻超過UI上限
hts-vs-expand-ceiling-warning-body = 要求的門檻{ $requested }超過UI上限，因此未附加該標頭。
hts-vs-expand-ceiling-value = 上限：{ $ceiling }
hts-vs-expand-too-costly-title = 展開因過於耗費而被拒絕
hts-vs-expand-too-costly-body = HTS拒絕執行超過目前門檻的展開。請在下方提高門檻後重新執行，或縮小篩選範圍。
hts-vs-expand-raise-threshold = 將門檻提高至
hts-vs-expand-raise-submit = 重試
hts-vs-expand-tree-label = 顯示完整樹狀結構，{ $count ->
    [one] { $count }個葉節點
   *[other] { $count }個葉節點
}
hts-vs-expand-total-label = 總數{ $total }
hts-vs-expand-total-unknown = 總數（未知）
hts-vs-expand-offset-label = offset { $offset }
hts-vs-expand-filter-no-match = 沒有成員符合篩選條件「{ $filter }」。
hts-vs-expand-no-members = 此展開結果不含任何成員。
hts-vs-expand-column-code = 代碼
hts-vs-expand-column-display = 顯示名稱
hts-vs-expand-column-system = 代碼系統
hts-vs-expand-load-more = 載入更多
hts-vs-expand-echoed-parameters = 回傳的參數

## HTS Slice D — ConceptMap browser + detail with embedded $translate
## workbench (design doc §7.5). Every key here has a peer in
## es/de/main.ftl.

## ConceptMap status pills.

hts-cm-status-draft = 草稿
hts-cm-status-active = 有效
hts-cm-status-retired = 已停用
hts-cm-status-unknown = 未知

## CM browser page.

hts-cm-browser-title = ConceptMap
hts-cm-browser-subtitle = 瀏覽術語伺服器的ConceptMap目錄，開啟任一列即可檢視其中繼資料或執行轉譯。
hts-cm-browser-filter-legend = 篩選ConceptMap
hts-cm-browser-filter-url = 正準URL
hts-cm-browser-filter-name = 名稱
hts-cm-browser-filter-title = 標題
hts-cm-browser-filter-status = 狀態
hts-cm-browser-filter-hint = 不提供以來源與目標正準URL篩選：搜尋ConceptMap時，HTS只接受url、version、name、title與status，其餘參數一律忽略。請先依URL或名稱篩選，再查看「對應」欄。
hts-cm-browser-filter-reset = 重設
hts-cm-browser-empty = 沒有符合這些篩選條件的ConceptMap。
hts-cm-browser-load-more = 載入更多
hts-cm-browser-showing-count = 顯示{ $count ->
    [one] { $count }個ConceptMap
   *[other] { $count }個ConceptMap
}
hts-cm-browser-table-caption = 符合目前篩選條件的ConceptMap。
hts-cm-browser-column-url = URL
hts-cm-browser-column-title = 標題
hts-cm-browser-column-status = 狀態
hts-cm-browser-column-name = 名稱
hts-cm-browser-column-source = 來源代碼系統
hts-cm-browser-column-target = 目標代碼系統
hts-cm-browser-column-mapping = 對應
hts-cm-browser-mapping-source-prefix = 來源：
hts-cm-browser-mapping-target-prefix = 目標：

## CM detail page.

hts-cm-detail-title = { $name } · ConceptMap
hts-cm-detail-title-fallback = ConceptMap
hts-cm-detail-eyebrow = ConceptMap
hts-cm-detail-section-identity = 識別資訊
hts-cm-detail-section-mapping = 對應
hts-cm-detail-publisher = 發行者
hts-cm-detail-jurisdiction = 適用地區
hts-cm-detail-purpose = 用途
hts-cm-detail-source-uri = 來源
hts-cm-detail-target-uri = 目標
hts-cm-detail-group-count = 群組數
hts-cm-detail-tabs-label = ConceptMap工作台區段
hts-cm-detail-tab-translate = 轉譯
hts-cm-detail-result-empty = 執行操作後，結果會顯示在這裡。

## CM $translate form + result labels.

hts-cm-translate-heading = 轉譯代碼
hts-cm-translate-scoped-map = ConceptMap（固定）
hts-cm-translate-direction-legend = 方向
hts-cm-translate-direction-forward = 正向
hts-cm-translate-direction-reverse = 反向
hts-cm-translate-source-legend = 來源Coding
hts-cm-translate-source-system = 代碼系統
hts-cm-translate-source-system-placeholder = 正準URL
hts-cm-translate-source-code = 代碼
hts-cm-translate-source-display = 顯示名稱
hts-cm-translate-source-display-placeholder = 選填
hts-cm-translate-reverse-legend = 反向來源
hts-cm-translate-target-code = 目標代碼
hts-cm-translate-target-code-hint = 反向模式下為必填。
hts-cm-translate-target-legend = 目標限制
hts-cm-translate-target-system = 目標代碼系統
hts-cm-translate-target-system-placeholder = 正準URL
hts-cm-translate-source-url = 來源ValueSet
hts-cm-translate-source-url-placeholder = 正準URL（選填）
hts-cm-translate-target-url = 目標ValueSet
hts-cm-translate-target-url-placeholder = 正準URL（選填）
hts-cm-translate-date = 日期
hts-cm-translate-date-placeholder = ISO 8601（例如2025-06-01）
hts-cm-translate-submit = 轉譯
hts-cm-translate-matches-count = { $count ->
    [one] { $count }筆符合結果
   *[other] { $count }筆符合結果
}
hts-cm-translate-no-matches = 此來源沒有符合的結果。
hts-cm-translate-column-code = 代碼
hts-cm-translate-column-system = 代碼系統
hts-cm-translate-column-display = 顯示名稱
hts-cm-translate-column-mapping = { $kind ->
    [equivalence]等價關係
    [relationship]關係
   *[other]對應
}
hts-cm-translate-column-origin = 出處

## HTS Slice E -- standalone Operations workbench (design doc s7.6).
## Every user-visible string on `/ui/hts/operations` resolves to a key
## in this section. Keys have peers in es/de/main.ftl (parity gated by
## the fluent-key inventory test).

## Shell.

## Op selector labels -- one per OperationKind slug.

## CS $lookup widening (Slice E adds useSupplement to the Slice B set).

## CS $validate-code widening.

## CS $subsumes standalone (heading + outcomes already live in Slice B).

## VS $expand widening (adds designation chip).
hts-vs-expand-advanced = 進階參數
hts-vs-expand-total = 總數{ $n }

## VS $validate-code (new op in Slice E).

## CM $translate standalone (base keys already live in Slice D).

## $closure workbench (new op in Slice E).

## batch-validate workbench (new UI-fabricated op in Slice E).

## Slice F — standalone Import page (design doc §7.7).
##
## Shell + upload form + status region for POST /import. All strings
## live under `hts-import-*`; `hts-nav-import` above is the sidebar
## label reused from the Phase 1 stub set.

hts-import-title = 匯入術語
hts-import-heading = 匯入術語
hts-import-help = 送出一份FHIR JSON Bundle。HTS可在單次POST中接受CodeSystem、ValueSet與ConceptMap資源。
hts-import-source-legend = 來源
hts-import-source-paste = 貼上JSON
hts-import-source-file = 上傳檔案
hts-import-bundle-textarea-label = FHIR Bundle (JSON)
hts-import-bundle-file-label = Bundle檔案(JSON)
hts-import-submit = 匯入
hts-import-status-empty = 尚未送出任何匯入。
hts-import-status-success = 匯入完成
hts-import-status-partial = 匯入部分成功
hts-import-status-rejected = 匯入遭拒
hts-import-status-too-large = Bundle過大
hts-import-counts-heading = 各資源筆數
hts-import-counts-created = 已建立／已更新
hts-import-resource-code-system = CodeSystem
hts-import-resource-value-set = ValueSet
hts-import-resource-concept-map = ConceptMap
hts-import-resource-concept = 已插入的概念
hts-import-issues-heading = { $n ->
    [one] { $n }個問題
   *[other] { $n }個問題
}
hts-import-too-large-hint = 請求超過伺服器的負載大小上限。請將Bundle拆成較小的批次後重試。
hts-import-empty-bundle-error = 送出前請先貼上JSON Bundle。
hts-import-invalid-json-error = 送出的內容不是有效的JSON。

# V3 "stepped" Import layout (#551): three numbered steps — choose source,
# review, result. Step 2 deliberately carries no entry counts: HTS reports
# counts only in the `POST /import` response, so a pre-flight number would
# be invented rather than measured.
hts-import-step-source = 選擇來源
hts-import-step-review = 檢閱
hts-import-step-result = 結果
hts-import-file-hint = 僅限JSON。檔案會在瀏覽器中讀取並複製到下方的Bundle欄位；在送出之前不會傳送任何資料。
hts-import-drop-hint = 將Bundle JSON檔案拖放到此處
hts-import-drop-browse = 或點擊以瀏覽檔案
hts-import-reading = 正在讀取檔案…
hts-import-read-failed = 無法讀取檔案。
hts-import-file-too-large = 檔案超過urlencoded送出方式約7.5 MiB的上限。
hts-import-file-loaded = 已載入檔案：{"{name}"}
hts-import-bundle-hint = 此Bundle會送至術語伺服器的POST /import。既有資源依url + version進行比對。
hts-import-review-target = 目標伺服器
hts-import-review-request = 請求
hts-import-review-accepted = 接受的資源
hts-import-review-accepted-value = CodeSystem、ValueSet、ConceptMap
hts-import-review-existing = 既有資源
hts-import-review-existing-value = 當url與version相符時就地更新。
hts-import-review-note = 送出前不會寫入任何資料。實際建立的資源數量由伺服器在下方結果中回報。
hts-import-counts-resource = 資源
hts-import-raw-toggle = 原始回應
hts-import-rejected-note = 未對術語儲存庫寫入任何資料。
hts-import-tag-success = 成功
hts-import-tag-partial = 部分成功
hts-import-tag-error = 錯誤

## Slice G — standalone Diagnostics page (design doc §7.9).
##
## Stacked-card view over CapabilityStatement, TerminologyCapabilities,
## /health, and /metrics — mirrors HFS's Capability Statement page.
## A failing source renders an `hts-degraded-reason-*` warning notice
## inside its own card; the other cards stay readable.


# Concept information plane (Direction B, "concept-first").
# The concept is a top-level object with its own permalink at
# /ui/hts/concepts?system=...&code=..., rendered as three panels:
# Identity, Mappings (across every stored ConceptMap), and Subsumption.
hts-concept-title = 概念
hts-concept-lede = 從術語伺服器能回答的每個角度檢視同一個代碼：它是什麼、對應到什麼，以及它在階層中的位置。
hts-concept-open = 開啟概念
hts-concept-panel-loading = 載入中
hts-concept-panel-open = 開啟此面板

hts-concept-identity-heading = 識別資訊
hts-concept-status-active = 有效
hts-concept-status-inactive = 無效
hts-concept-status-unreported = 未回報有效狀態
hts-concept-field-system = 代碼系統
hts-concept-field-code = 代碼
hts-concept-field-display = 顯示名稱
hts-concept-field-code-system-name = CodeSystem名稱
hts-concept-field-version = 版本
hts-concept-field-selectability = 可選取性
hts-concept-selectability-abstract = 抽象（不可選取）
hts-concept-selectability-selectable = 可選取
hts-concept-field-definition = 定義
hts-concept-field-neighbours = 階層鄰接概念
hts-concept-field-used-supplements = 已套用的Supplement
hts-concept-designations-heading = 名稱設計（designation）
hts-concept-designations-value = 名稱設計
hts-concept-designations-language = 語言
hts-concept-designations-use = 用途
hts-concept-properties-heading = 屬性
hts-concept-properties-code = 屬性
hts-concept-properties-value = 值
hts-concept-raw-response = 原始回應

hts-concept-mappings-heading = 對應
hts-concept-mappings-direction-forward = 以此概念為來源的對應，涵蓋所有已儲存的ConceptMap。
hts-concept-mappings-direction-reverse = 以此概念為目標的對應，涵蓋所有已儲存的ConceptMap。
hts-concept-mappings-switch-forward = 顯示從此概念出發的對應
hts-concept-mappings-switch-reverse = 顯示對應到此概念的對應
hts-concept-mappings-empty = 沒有任何ConceptMap對應此概念。
hts-concept-mappings-vocabulary = 對應詞彙
hts-concept-mappings-vocabulary-equivalence = equivalence (R4 / R4B)
hts-concept-mappings-vocabulary-relationship = relationship (R5 / R6)
hts-concept-mappings-vocabulary-unknown = 未回報
hts-concept-mappings-unattributable = 伺服器不會將反向模式的符合結果歸屬到特定的來源對應表，因此無法顯示出處。請切換到正向方向，以查看每筆對應來自哪個ConceptMap。
hts-concept-mappings-origin = 出處ConceptMap
hts-concept-mappings-column-code = 代碼
hts-concept-mappings-column-system = 代碼系統
hts-concept-mappings-column-display = 顯示名稱
hts-concept-mappings-column-mapping = 關係

hts-concept-relations-heading = 包含關係（subsumption）
hts-concept-relations-lede = 每一列是一次包含關係檢查。祖先候選一律作為代碼A送出，因此前後一致的階層每次都會回答「subsumes」。
hts-concept-relation-parent = 上層
hts-concept-relation-child = 下層
hts-concept-relation-manual = 手動比較
hts-concept-relations-column-relation = 關係
hts-concept-relations-column-question = 檢查問題
hts-concept-relations-column-outcome = 結果
hts-concept-relations-subsumes-verb = 包含
hts-concept-subsumes-outcome-equivalent = 等價
hts-concept-subsumes-outcome-subsumes = 包含
hts-concept-subsumes-outcome-subsumed-by = 被包含
hts-concept-subsumes-outcome-not-subsumed = 無包含關係
hts-concept-relations-conflict-caveat = 概念查詢回報了這條階層連結，但包含關係檢查並未確認。這通常表示CodeSystem重新匯入後未重建包含關係closure；階層本身仍然完整。
hts-concept-relations-empty = 此概念沒有可比較的上層或下層概念。
hts-concept-relations-dropped = 另有{ $n }個比較對象未檢查；此面板每次呈現最多執行20次包含關係呼叫。
hts-concept-relations-compare-label = 與代碼比較
hts-concept-relations-compare-placeholder = 此代碼系統中的另一個代碼
hts-concept-relations-compare-hint = 代碼系統已固定為此概念所屬的系統，因此只需輸入代碼本身。此檢查會詢問該代碼是否包含本概念。
hts-concept-relations-compare-submit = 比較

## HTS detail pages -- V3 compact header (#551 Slice B/C/D layout pass).
## Shared chip-row and disclosure labels for the CodeSystem / ValueSet /
## ConceptMap detail pages, plus the result-panel headings and the two
## honesty footnotes (tree pager, reverse-mode originMap).

hts-detail-facts-label = 基本資料
hts-detail-canonical-url = 正準URL
hts-detail-version-label = 版本
hts-detail-status-label = 狀態
hts-cs-detail-facts-summary = 所有CodeSystem基本資料
hts-vs-detail-facts-summary = 所有ValueSet基本資料
hts-cm-detail-facts-summary = 所有ConceptMap基本資料
hts-cs-lookup-definition = 定義
hts-cs-validate-result-heading = 驗證結果
hts-cs-subsumes-result-heading = 包含關係結果
hts-vs-expand-result-heading = 展開結果
hts-vs-expand-table-caption = 術語伺服器回傳的展開結果成員。
hts-vs-expand-tree-note = 樹狀模式會回傳整個階層；分頁僅適用於平面模式。
hts-cm-translate-table-caption = 術語伺服器回傳的轉譯符合結果。
hts-cm-translate-origin-reverse-note = 在反向模式下HTS不會提供originMap，因此無法將符合結果歸屬到特定的概念對應。每個「出處」儲存格刻意維持為破折號——這並不是缺漏的值。


# Capability & Conformance page (HTS mirror of HFS's page). The shared
# `cap-*` and `nav-capability-conformance` keys carry everything both
# pages say identically; only what is specific to a terminology server
# lives here.
hts-capability-lede = 此術語伺服器目前對外宣告的能力——即時由/metadata組成。
hts-capability-operations-empty = 未宣告任何操作。
hts-capability-rest-empty = 未宣告任何REST資源。
hts-capability-terminology-heading = 術語能力
hts-capability-expansion-hierarchical = 階層式展開
hts-capability-expansion-paging = 展開結果分頁
hts-capability-expansion-incomplete = 不完整的展開結果
hts-capability-expansion-parameters = $expand參數
hts-capability-validate-code-translations = 驗證代碼時的轉譯
hts-capability-translation-needs-map = 轉譯需要對應表
hts-capability-closure = Closure維護
hts-capability-code-systems-declared = 已宣告的代碼系統
hts-capability-flag-true = 是
hts-capability-flag-false = 否

# Home V3 tile sub-lines. The mockup folds Backend, FHIR version,
# Bundled data and Avg latency into the sub-line of the tile each
# qualifies, instead of giving them tiles of their own.
hts-home-tile-status-sub = 後端{ $backend } · FHIR { $version }
hts-home-tile-uptime-sub = hts v{ $version } · 自{ $since } UTC起未曾重啟
hts-home-tile-uptime-sub-noclock = hts v{ $version }
hts-home-tile-loaded-systems-sub = 磁碟上的隨附資料{ $mib } MiB
hts-home-tile-requests-sub = 平均{ $ms } ms · 來自/metrics

# Chart caption, composed from the SELECTED window and status class.
# Each locale owns its own word order through the two placeables.
hts-home-chart-hint = { $window }，{ $classes }。在此頁面開啟期間取樣。不含本頁自身每15秒的重新整理與/metrics的抓取。
hts-home-chart-hint-window-15m = 最近15分鐘
hts-home-chart-hint-window-1h = 最近1小時
hts-home-chart-hint-window-6h = 最近6小時
hts-home-chart-hint-series-all = 所有狀態類別
hts-home-chart-hint-series-2xx = 僅2xx回應
hts-home-chart-hint-series-4xx = 僅4xx回應
hts-home-chart-hint-series-5xx = 僅5xx回應

## Bearer-only authentication (#1560): auth is on, no browser sign-in configured
auth-bearer-only = 此伺服器已啟用身分驗證，但未設定瀏覽器登入。從瀏覽器呼叫FHIR API的頁面——資源、批次／交易、已儲存的查詢與偏好設定——會被以401拒絕。請設定HFS_UI_LOGIN_CLIENT_ID以啟用登入，或直接以bearer token使用API。
batch-sign-in-required = 此伺服器未設定瀏覽器登入，因此本頁面無法為請求進行身分驗證。請設定HFS_UI_LOGIN_CLIENT_ID，才能從此處執行bundle。
batch-too-large = 此bundle超過伺服器可接受的大小。請將其拆成較小的bundle，或提高伺服器的HFS_MAX_BODY_SIZE。

## 批次 / 交易：貼上模式與用戶端檢查 (p1-ui)
batch-mode-label = Bundle 輸入方式
batch-mode-file = 上傳檔案
batch-mode-paste = 貼上 JSON
batch-paste-label = Bundle JSON
batch-paste-placeholder = 在此貼上 batch 或 transaction Bundle…
batch-paste-hint = 輸入時即時檢查；每個問題都會標示行號與欄號。
batch-format = 格式化
batch-format-failed = 請先修正 JSON 語法錯誤再格式化。
batch-continue = 檢視執行計畫
batch-check-ok = 未發現問題。
batch-check-errors = 錯誤
batch-check-warnings = 警告
batch-check-by-method = 依方法
batch-check-by-type = 依資源類型
batch-check-entries = 共 %count% 個項目
batch-check-blocked = 請修正錯誤後再繼續；警告不會阻擋送出。
batch-check-at = 第 %line% 行，第 %col% 欄

## 共用 Bundle 檢查 (bundle-check.js)。%name% 佔位符由瀏覽器填入。
bundle-check-empty = 尚無內容可檢查：請貼上或上傳 Bundle。
bundle-check-json-syntax = JSON 語法錯誤：%detail%。
bundle-check-json-eof = JSON 尚未完整，文字就已結束
bundle-check-json-unexpected = 非預期的字元 %char%
bundle-check-json-expected = 此處應為 %expected%
bundle-check-not-object = JSON 必須是單一資源物件。
bundle-check-not-bundle = resourceType 為「%found%」；batch 或 transaction 必須是 Bundle。
bundle-check-missing-type = 缺少 Bundle.type：請設為 batch 或 transaction。
bundle-check-bad-type = Bundle.type 為「%found%」。只有 batch 與 transaction 類型的 Bundle 可以送出，因為只有它們的每個項目都帶有請求（method 與 url）。
bundle-check-entry-not-array = Bundle.entry 必須是陣列。
bundle-check-no-entries = 這個 Bundle 沒有任何項目，送出後不會執行任何動作。
bundle-check-entry-not-object = 項目 %entry% 不是 JSON 物件。
bundle-check-no-request = 項目 %entry% 沒有 request：batch 或 transaction 的每個項目都需要 request.method 與 request.url。
bundle-check-no-method = 項目 %entry%：缺少 request.method。
bundle-check-bad-method = 項目 %entry%：request.method「%method%」不是 GET、POST、PUT、PATCH、DELETE、HEAD 其中之一。
bundle-check-no-url = 項目 %entry%：缺少 request.url。
bundle-check-bad-url = 項目 %entry%：%method% 的 request.url 應為 Type/id 格式（條件式 %method% 則為 Type?search），而不是「%url%」。
bundle-check-no-resource = 項目 %entry%：%method% 項目必須包含 resource。
bundle-check-no-resource-type = 項目 %entry%：resource 沒有 resourceType。
bundle-check-url-type-mismatch = 項目 %entry%：request.url 指向 %url_type%，但資源是 %type%。
bundle-check-id-mismatch = 項目 %entry%：request.url 的 id 為「%url_id%」，但 resource.id 為「%id%」。
bundle-check-duplicate-full-url = 項目 %entry%：fullUrl %fullurl% 也被項目 %other% 使用。
bundle-check-unresolved-urn = 項目 %entry%：參照 %ref% 在此交易中找不到對應 fullUrl 的項目。
bundle-check-too-large = 此 Bundle 大小為 %size%，超過伺服器可接受的上限（%limit%）。請拆分，或在伺服器上調高 HFS_MAX_BODY_SIZE。

## 驗證但不匯入 ($validate)：批次頁與驗證頁
nav-validate = 驗證
validate-run = 驗證
validate-run-hint = 在伺服器上檢查此 Bundle（$validate），不會匯入任何資料。
validate-per-entry = 逐一驗證每個項目
validate-per-entry-hint = 另外對每個項目的資源執行 $validate，並依其 meta.profile 驗證。
validate-running = 驗證中…
validate-progress = 正在驗證第 %done% 個項目，共 %total% 個…
validate-result-heading = 驗證結果
validate-heading-bundle = Bundle
validate-heading-resource = 資源
validate-heading-entries = 逐一驗證的項目
validate-verdict-ok = 有效：沒有錯誤或警告
validate-verdict-errors = 錯誤：%errors% · 警告：%warnings%
validate-verdict-warnings = 有效，但有警告：%warnings%
validate-group-error = 錯誤
validate-group-warning = 警告
validate-group-information = 資訊
validate-sev-fatal = 嚴重
validate-sev-error = 錯誤
validate-sev-warning = 警告
validate-sev-information = 資訊
validate-col-severity = 嚴重度
validate-col-location = 位置
validate-col-code = 代碼
validate-col-message = 訊息
validate-entry-label = 項目 %n%（%name%）
validate-entry-label-bare = 項目 %n%
validate-request-failed = 驗證請求失敗（%detail%）。
validate-page-heading = 驗證
validate-page-lede = 依 FHIR 規則與此伺服器已知的 profile 檢查任何資源或 Bundle，不會儲存任何資料。
validate-input = 資源
validate-input-label = 資源或 Bundle JSON
validate-input-placeholder = 貼上 FHIR 資源或 Bundle…
validate-profile = Profile（選填）
validate-profile-placeholder = 例如 https://twcore.mohw.gov.tw/ig/twcore/StructureDefinition/Patient-twcore
validate-profile-hint = StructureDefinition 的 canonical URL；清單列出此伺服器上儲存的 profile。資源本身的 meta.profile 一律會一併檢查。
validate-profiles-loaded = 共有 %count% 個 profile 可選
