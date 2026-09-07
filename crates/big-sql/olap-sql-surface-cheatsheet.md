# OLAP SQL Surface — Cheatsheet đầy đủ

Toàn bộ bề mặt SQL mà một OLAP database "nên có", kèm cột ưu tiên để cắt scope cho bitlas.

**Priority:** `P0` = không có thì không gọi là OLAP · `P1` = cần trước khi ai đó dùng thật · `P2` = khi cạnh tranh với ClickHouse/Doris · `P3` = có thì tốt · `✗` = đừng làm.

---

## 1. Bản đồ statement

| Nhóm | Statement | P | Ghi chú cho engine bitmap |
|---|---|---|---|
| DDL | CREATE/DROP DATABASE, TABLE | P0 | ✅ xong. DATABASE = namespace, không phải storage boundary |
| DDL | ALTER DATABASE | ✗ | database chỉ mang một cái tên |
| DDL | CREATE/DROP VIEW | P0 | ✅ xong. Statement lưu trong catalog, inline vào câu đọc nó |
| DDL | CREATE MATERIALIZED VIEW / ROLLUP | P2 | |
| DDL | CREATE INDEX | ✗ | bitmap đã là index |
| DDL | CREATE FUNCTION (UDF/UDAF) | P3 | |
| DDL | CREATE CATALOG (external: Iceberg/Hive/JDBC) | P3 | |
| DML | INSERT VALUES / INSERT SELECT | P0 | ✅ xong. `INSERT SELECT` nhận **projection** — dạng duy nhất đọc ra giá trị theo record. Câu nguồn chạy trọn vẹn qua đúng đường một `SELECT` thường trước khi ghi fact đầu tiên |
| DML | INSERT OVERWRITE (partition/table) | P2 | |
| DML | UPDATE / DELETE theo predicate | P1 | ✅ cả hai. `DELETE` **không** sinh delete bitmap — bit xoá thật, id tái dùng được. `UPDATE` chỉ trên cột mà một record giữ **một** giá trị (int/decimal/float/`MUTEX`/`BOOL`); `SET`/`TIMEQUANTUM` bị từ chối vì ghi thêm chứ không thay |
| DML | MERGE INTO / upsert | P3 | |
| DML | TRUNCATE, LOAD/COPY INTO, STREAM LOAD | P1 | ✅ `TRUNCATE TABLE [IF EXISTS]` xong — là **DDL** chứ không phải write: giải phóng fragment theo khoá, đi đường leader-rồi-fanout. `COPY INTO` chưa; stream load đã có ở `POST /table/{t}/import` |
| DQL | SELECT (đầy đủ, mục 4) | P0 | |
| Utility | EXPLAIN | done | `EXPLAIN [PLAN\|SHAPE] <statement>`; ANALYZE still P2 |
| Utility | SHOW, DESCRIBE, system tables | P1 | ✅ `SHOW`/`DESCRIBE` xong; `system.tables/columns/databases/parts` xong — xem §11 |
| Utility | ANALYZE TABLE (statistics) | P2 | |
| Utility | OPTIMIZE / COMPACT | P2 | |
| Session | SET, SHOW VARIABLES, KILL QUERY | P1 | `SET`/`SHOW VARIABLES` ✗ (`SETTINGS` thay thế, §12). `KILL QUERY '<node>/<n>'` ✅ và `SHOW PROCESSLIST` ✅ — cả hai **node-local**, trả lời ở edge vì cờ huỷ được tạo ở đó |
| TCL | BEGIN/COMMIT/ROLLBACK | P3 | OLAP thường auto-commit per statement |
| DCL | GRANT/REVOKE, CREATE/DROP ROLE, SHOW ROLES/GRANTS | ✅ | `CREATE USER` refused: người dùng nằm trong users file, ghi bằng `big passwd`. Xem `docs/access-control.md` |

---

## 2. DDL — cú pháp đầy đủ

### 2.1 CREATE TABLE

```sql
CREATE TABLE [IF NOT EXISTS] [db.]tbl
(
    col1  TYPE  [NOT NULL] [DEFAULT expr] [CODEC(ZSTD(3))] [COMMENT '...'],
    col2  TYPE  MATERIALIZED expr,           -- computed, lưu
    col3  TYPE  ALIAS expr,                  -- computed, không lưu
    INDEX idx col1 TYPE bloom_filter GRANULARITY 4
)
ENGINE       = MergeTree                     -- hoặc data model: DUPLICATE/AGGREGATE/UNIQUE KEY
PARTITION BY toYYYYMM(ts)                    -- pruning coarse
ORDER BY     (tenant_id, ts)                 -- sort key / sparse primary index
PRIMARY KEY  (tenant_id)                     -- prefix của ORDER BY
DISTRIBUTED BY HASH(user_id) BUCKETS 32      -- phân tán
SAMPLE BY    intHash64(user_id)
TTL          ts + INTERVAL 90 DAY
SETTINGS     index_granularity = 8192
COMMENT      '...';
```

| Mệnh đề | P | Bitlas map |
|---|---|---|
| Column list + type | P0 | → field + FieldKind |
| `DEFAULT` / `MATERIALIZED` / `ALIAS` | P2 | |
| `CODEC` per column | P3 | roaring đã tự chọn container |
| `PARTITION BY` | ✗ | shard = `record_id >> 20`, ẩn |
| `ORDER BY` / sort key | ✗ | không có row order; record_id là thứ tự |
| `DISTRIBUTED BY ... BUCKETS` | P2 | shard là đơn vị phân tán tự nhiên |
| `TTL` | ✗ | **Từ chối có tên** (`sql_declarative_ttl`): không có scheduler, và `big-embed` không tạo thread. Thay bằng `ALTER TABLE t DROP DAYS BEFORE '<date>' ON <col>` ✅ — và nó bỏ *chỉ mục* theo chu kỳ, không bỏ record |
| `SAMPLE BY` | P2 | rẻ với bitmap: subset container |

### 2.2 Biến thể CREATE TABLE

```sql
CREATE TABLE t2 LIKE t1;                     -- copy schema
CREATE TABLE t2 AS SELECT ... ;              -- CTAS                          P1
CREATE TABLE t2 ENGINE=... AS t1;            -- schema + engine
CREATE TEMPORARY TABLE ...                   --                               P2
CREATE TABLE t (...) ENGINE = MySQL(...)     -- external table                P3
```

### 2.3 ALTER TABLE — bảng chi phí

| Lệnh | Chi phí thật | P |
|---|---|---|
| `ADD COLUMN c TYPE [AFTER x]` | metadata | ✅ |
| `DROP COLUMN c` | metadata + GC | ✅ |
| `RENAME COLUMN a TO b` | metadata (nếu path theo id) | P1 |
| `MODIFY COLUMN c TYPE` | **rewrite toàn bộ** | P2 |
| `MODIFY COLUMN c COMMENT/DEFAULT/CODEC` | metadata | P2 |
| `ADD/DROP PARTITION` | metadata + GC | P1 |
| `MODIFY TTL` | metadata + background drop | ✗ `sql_declarative_ttl` |
| `ADD INDEX` / `MATERIALIZE INDEX` | build async | ✗ |
| `UPDATE ... WHERE` (mutation) | ghi fact mới + xoá fact cũ, đồng bộ | ✅ |
| `DELETE WHERE` | xoá bit thật, không delete bitmap | ✅ |
| `ADD PROJECTION` | build async | P3 |
| `RENAME TO` | metadata | P1 |
| `EXCHANGE TABLES a AND b` | atomic swap — rất hữu ích cho rebuild | P2 |

### 2.4 View & Materialized View

```sql
CREATE VIEW v AS SELECT <cột> [AS tên], ... FROM t [WHERE ...];        -- ✅ xong

CREATE MATERIALIZED VIEW mv
[TO target_tbl]                              -- ClickHouse: MV là insert trigger
[REFRESH EVERY 1 HOUR]                       -- refresh-based (Doris/Snowflake)
AS SELECT tenant, toDate(ts) d, count() c, uniqState(uid) u
   FROM t GROUP BY tenant, d;                                         -- P2

ALTER MATERIALIZED VIEW mv REFRESH;
DROP MATERIALIZED VIEW mv;
```

**View thường: body chỉ được là lọc + chiếu trên một bảng.** Không phải cắt scope cho nhanh — đó là
đúng cái inline được. View ở đây được *thay thẳng* vào câu đọc nó: bảng gốc thế chỗ tên view, hai
`WHERE` AND lại, cột của câu ngoài đổi tên qua select list của body. Dưới tầng này không có subquery
để lồng, nên body có `GROUP BY` hay hàm tổng hợp thì không có câu lệnh nào để trở thành — câu trả lời
của nó phải tồn tại *trước khi* câu ngoài chạy, và đó chính là MV.

Lưu ý dễ vấp: **`*` ở đây là record id, không phải "mọi cột"** — nên body phải kể tên cột nó mở ra.
Cột không kể tên thì không đọc được qua view (`sql_view_column`), và đó là phần lớn lý do người ta
tạo view.

Đừng nhầm với *bitmap view* trong `FragmentKey = (table, field, view, shard)`: cái đó là phân mảnh
bitmap theo time quantum, không liên quan gì đến SQL view. Trong `big-db` nó tên là `SavedQuery`
riêng để hai thứ không đứng cạnh nhau dưới một chữ.

Hai mô hình MV, chọn một, đừng trộn:

| Mô hình | Cơ chế | Ví dụ | Ưu / nhược |
|---|---|---|---|
| Incremental (insert-trigger) | mỗi block insert → chạy SELECT → append vào target | ClickHouse | Realtime, nhưng không backfill, không xử lý được UPDATE/DELETE nguồn |
| Refresh / rollup đồng bộ | engine tự duy trì, query rewrite tự chọn | Doris, StarRocks, Snowflake | Trong suốt với user, đắt lúc ghi |

---

## 3. DML

```sql
INSERT INTO t (a,b) VALUES (1,'x'), (2,'y');                          -- P0
INSERT INTO t (a,b) SELECT a, b FROM s WHERE ...;                     -- ✅ projection
INSERT INTO t FORMAT JSONEachRow / Parquet / CSV                      -- P1
INSERT OVERWRITE TABLE t PARTITION (dt='2026-09-01') SELECT ...       -- P2

UPDATE t SET a = a+1 WHERE tenant = 5;                                -- P1
DELETE FROM t WHERE ts < '2026-01-01';                                -- ✅ xong
TRUNCATE TABLE t;                                                     -- ✅ xong

MERGE INTO tgt USING src ON tgt.id = src.id
  WHEN MATCHED THEN UPDATE SET ...
  WHEN NOT MATCHED THEN INSERT ...;                                   -- P3

COPY INTO t FROM 's3://.../*.parquet' FILE_FORMAT = (TYPE=PARQUET);   -- P2
```

Ngữ nghĩa OLAP cần ghi rõ trong docs, khác OLTP:

| Điểm | Hành vi chuẩn OLAP |
|---|---|
| `UPDATE`/`DELETE` | Bất đồng bộ, không đảm bảo thấy ngay ở query kế tiếp |
| Atomicity | Per-statement, thường per-block; không có multi-statement txn |
| Duplicate | Không có PK enforcement trừ khi dùng UNIQUE/Replacing model |
| Insert nhỏ | Kẻ thù số 1 — cần batching, mọi engine đều khuyến nghị ≥ 1,000 row/insert |

---

## 4. DQL — SELECT đầy đủ

### 4.1 Grammar

```sql
[WITH cte AS (...), RECURSIVE r AS (...)]
SELECT [DISTINCT | DISTINCT ON (expr)]
       expr [AS alias], ...
FROM      tbl [FINAL] [SAMPLE 0.1] [AS t] [FOR SYSTEM_TIME AS OF ...]
[join_clause ...]
[ARRAY JOIN | LATERAL VIEW explode(...)]
[PREWHERE expr]                      -- ClickHouse: lọc trước khi đọc cột khác
[WHERE expr]
[GROUP BY expr, ... [WITH ROLLUP | WITH CUBE | GROUPING SETS (...)] [WITH TOTALS]]
[HAVING expr]
[WINDOW w AS (PARTITION BY ... ORDER BY ... frame)]
[QUALIFY expr]                       -- lọc theo kết quả window function
[ORDER BY expr [ASC|DESC] [NULLS FIRST|LAST] [WITH FILL]]
[LIMIT n [OFFSET m]] | [LIMIT n BY expr] | [OFFSET n ROWS FETCH NEXT m ROWS ONLY]
[SETTINGS k = v]
[UNION ALL | UNION DISTINCT | INTERSECT | EXCEPT ...]
```

### 4.2 Thứ tự đánh giá logic

```
FROM/JOIN → PREWHERE → WHERE → GROUP BY → HAVING → WINDOW → QUALIFY
          → SELECT → DISTINCT → ORDER BY → LIMIT
```

### 4.3 Ưu tiên

| Mệnh đề | P | Ghi chú |
|---|---|---|
| `SELECT`, `FROM`, `WHERE`, `LIMIT` | P0 | ✅ xong. `PREWHERE` cũng nhận và gộp thẳng vào `WHERE`: ở đây nó chọn đúng tập mà `WHERE` chọn, vì không có hàng nào để đọc trước — một câu port từ ClickHouse chạy chứ không gãy vì một từ không đổi gì. `LIMIT ... WITH TIES` cũng có |
| `GROUP BY` + aggregate | P0 | ✅ xong, **tới 4 cột**, và trên hai loại cột chứ không phải một. Cột keyed thì group bằng cách đi hết từ điển; cột `DATE`/`DATETIME` **không có từ điển**, nên `GROUP BY date_trunc('month', ts)` là một plan riêng: bucket lấy từ *lịch*, mỗi bucket là một range trên mặt phẳng bit. Bucket rỗng không phải một nhóm, và record không có giá trị thì **không nằm trong bucket nào** — nên tổng các count ở đây là số record *có* giá trị, không phải `count(*)`. Trần là **số lượt đi**, kiểm ở đầu mỗi tầng: frontier sau tầng một chính là số lượt của tầng hai, nên một ngân sách chặn luôn cả tích |
| `ORDER BY` | P0 | ✅ xong trên grouped **và** trên projection. Trên projection cái giá là thật và đã nói rõ: sort phải thấy mọi hàng trước khi biết mười hàng nào sống, nên `LIMIT` rời khỏi plan và câu lệnh đọc mọi record khớp `WHERE`. Chặn bởi đúng trần `max_records` mà mọi phép đọc không giới hạn khác chịu. `NULL` xếp cuối ở cả hai chiều |
| `HAVING` | P0 | ✅ xong, **có hay không có `GROUP BY`**. Không nằm trong plan: ngưỡng kiểm ở coordinator sau merge, vì một tổng dưới ngưỡng ở một node có thể vượt khi cộng đủ. Chỉ được gọi tên con số mà select list đã hỏi |
| `DISTINCT` | P0 | ✅ xong, và không phải bằng một đường riêng: `SELECT DISTINCT a, b` **là** `GROUP BY a, b`, lower ra đúng cái cây đó. Nên nó thừa hưởng luôn trần 4 cột |
| CTE (`WITH`) không đệ quy | P1 | `WITH <hằng> AS <tên>` ✅ (buộc một giá trị); `WITH x AS (SELECT …)` thì `sql_unsupported` |
| Subquery (scalar, IN, EXISTS) | P1 | ✅ **đúng một dạng**: `IN (SELECT _record_id FROM t [WHERE …])` và `NOT IN` — semi/anti join. Còn lại từ chối |
| `UNION ALL` | P1 | ✅ xong, kể cả nhánh grouped và nhánh có join. `UNION`/`UNION DISTINCT` thì `sql_union`: bỏ trùng ở đây là so chuỗi đã render |
| `JOIN` (mục 5) | P1 | ✅ xong — xem mục 5 |
| Window function (mục 7) | P2 | |
| `WITH ROLLUP/CUBE/GROUPING SETS` | P2 | cực hợp bitmap — chia sẻ intermediate |
| `QUALIFY` | P3 | |
| `SAMPLE` | P2 | rẻ với roaring |
| `LIMIT n BY expr` | P3 | top-n per group, rất hay dùng |
| `ORDER BY ... WITH FILL` | P3 | điền gap time series |
| CTE đệ quy | P3 | |
| Time travel `AS OF` | P2 | gần như free nếu snapshot COW |

---

## 5. JOIN

| Loại | Cú pháp | P | Ghi chú |
|---|---|---|---|
| INNER / LEFT / RIGHT | chuẩn ANSI | ✅ | Nhiều bảng cũng được, miễn là một **sao** quanh một khoá chung. Không có `Plan` variant mới và không có merge arm mới — một join là một plan thường mỗi bảng, cộng một `Shape::Join` |
| FULL | `FULL [OUTER] JOIN` | ✅ | Chỉ khi **mọi** join trong câu đều là `FULL`; trộn với loại khác thì `sql_no_outer_joins` |
| CROSS | `CROSS JOIN` | ✗ | `sql_no_joins` — không gọi tên khoá nào để ghép |
| SEMI / ANTI | `IN (SELECT _record_id FROM …)` / `NOT IN` | ✅ | **Ngữ nghĩa đã có, cú pháp `LEFT SEMI JOIN` thì chưa.** Đúng là bitmap AND / ANDNOT như dự đoán. Chặn bởi `sql_set_too_large` |
| ASOF | `ASOF LEFT JOIN ... ON a.id=b.id AND a.t >= b.t` | P3 | time-series |
| ARRAY JOIN / LATERAL / UNNEST | `ARRAY JOIN arr` | P2 | |
| Colocated / bucket-shuffle | hint | P3 | |
| Broadcast vs shuffle | `SETTINGS join_algorithm=...` | P2 | |

Với engine bitmap, SEMI/ANTI join trên cột đã index là intersect/difference hai bitmap — đây là lợi thế cạnh tranh, ưu tiên hơn hash join tổng quát.

---

## 6. GROUP BY mở rộng

```sql
GROUP BY a, b WITH ROLLUP          -- (a,b), (a), ()
GROUP BY a, b WITH CUBE            -- (a,b), (a), (b), ()
GROUP BY GROUPING SETS ((a,b),(a),())
SELECT ..., grouping(a) ...        -- phân biệt NULL thật vs NULL do rollup
GROUP BY a WITH TOTALS             -- thêm 1 dòng tổng
```

| Số cột | ROLLUP sinh | CUBE sinh |
|---|---|---|
| 2 | 3 nhóm | 4 nhóm |
| 3 | 4 nhóm | 8 nhóm |
| 4 | 5 nhóm | 16 nhóm |
| n | n+1 | 2^n |

---

## 7. Window function

```sql
func() OVER (
  PARTITION BY a
  ORDER BY ts
  ROWS BETWEEN 6 PRECEDING AND CURRENT ROW     -- hoặc RANGE / GROUPS
)
```

| Họ | Hàm | P |
|---|---|---|
| Ranking | `row_number, rank, dense_rank, ntile, percent_rank, cume_dist` | P2 |
| Offset | `lag, lead, first_value, last_value, nth_value` | P2 |
| Aggregate window | `sum/avg/count/min/max ... OVER` | P2 |
| Frame | `ROWS` / `RANGE` / `GROUPS`, `EXCLUDE` | P2 / P3 |

---

## 8. Type system OLAP

| Nhóm | Type | P | Trạng thái |
|---|---|---|---|
| Integer | `Int8/16/32/64`, `UInt*` | P0 | ✅ xong (`TINYINT`…`BIGINT`, `UINT(n)`, `SIGNED(n)`); 128 bit thì không — một giá trị bit-sliced dừng ở 64 |
| Decimal | `Decimal(p,s)` | P0 | ✅ xong. Lưu dạng số nguyên có scale, không đi qua float ở bất kỳ đâu |
| Float | `Float32/64` | P0 | ✅ xong (`FLOAT`/`REAL`/`FLOAT32`, `DOUBLE`/`FLOAT64`). Lưu dưới một phép biến đổi bit giữ nguyên thứ tự, nên `WHERE`, `min`, `max` và zone map chạy y như mọi field bit-sliced. **`sum`/`avg` phải quét**: phép mã hoá không affine nên không cộng được theo mặt phẳng bit. Tiền vẫn dùng `DECIMAL(p,s)` — float không tròn số |
| String | `String`, `FixedString(n)`, `LowCardinality(String)` | P0 | ✅ `TEXT/VARCHAR/CHAR/STRING` → `Set`, và `LowCardinality(String)` cũng vậy: đó đúng là thứ một `SET` vốn đã là, một key intern một lần và một bitmap cho mỗi key. Chỉ trên chuỗi — `LowCardinality(Int64)` bị từ chối, vì một con số là mặt phẳng bit và không có từ điển nào để mà "ít giá trị". `FixedString` thì không — key lưu nguyên, không có gì để cắt hay đệm |
| Bool | `Bool` | P0 | ✅ xong |
| Date/Time | `Date`, `DateTime`, `DateTime64(p, tz)` | P0 | ✅ `DATE` (số ngày) và `DATETIME`/`TIMESTAMP` (số giây) là kiểu vô hướng có thứ tự: `WHERE d >= '2024-01-01'`, `ORDER BY`, `min`/`max` đều hỏi được. `TIMEQUANTUM` vẫn là field keyed theo view ngày, và giờ là tên riêng của nó — trước đây cả bốn cách viết đều là time quantum. Không timezone, không `DateTime64(p)`; `sum` bị từ chối vì tổng hai ngày không phải một ngày |
| Nullable | `Nullable(T)` | ✗ | **Từ chối có tên** (`sql_nullable_type`) — xem §15. Vắng mặt vốn đã là cách một cột trả lời ở đây |
| Enum | `Enum8/16` | P2 | |
| Composite | `Array(T)`, `Map(K,V)`, `Tuple(...)`, `Nested` | P2 | |
| Semi-structured | `JSON` / `Variant` / `Dynamic` | P3 | |
| UUID / IP | `UUID`, `IPv4`, `IPv6` | P2 | |
| **Bitmap** | `BITMAP` / `AggregateFunction(groupBitmap, UInt64)` | ✗ | **Từ chối có tên** (`sql_bitmap_type`) — mọi cột ở đây vốn đã là bitmap; xem §10 và §15 |
| Sketch | `HLL`, `AggregateFunction(uniq, ...)`, `QUANTILE_STATE` | P2 | |
| Geo | `Point`, `Polygon` | P3 | |

`LowCardinality` đáng chú ý: nó chính là dict-encode + là điều kiện để một cột string dùng được bitmap index. Với bitlas thì mọi SET field vốn đã là như vậy — nên expose type này ra SQL cho tự nhiên.

---

## 9. Function catalog

| Họ | Đại diện | P | Trạng thái |
|---|---|---|---|
| Aggregate cơ bản | `count, sum, avg, min, max` | P0 | ✅ xong |
| ~~Aggregate cần xem lại giá trị~~ | `any, argMin, argMax, stddev*, var*, corr` | ✗ | **Không phải P0, và không phải việc chưa làm.** Cả năm cần xem lại giá trị từng record đối chiếu một tổng đang chạy; engine giữ bit ở `(row, record)` chứ không giữ giá trị để xem lại, và `stddev`/`var`/`corr` còn cần tổng bình phương mà mặt phẳng bit không cộng được. `argMin(a, b)` viết được bằng `SELECT a, b FROM t ORDER BY b LIMIT 1` — cùng câu trả lời, và nói rõ nó tốn gì. Phải kể cả `b` trong select list, vì `ORDER BY` chỉ được gọi tên cột mà projection đã đọc |
| Conditional agg | `sumIf, countIf, avgIf` / `FILTER (WHERE ...)` | P1 | ✅ xong (khai thiếu ở bản trước) |
| Distinct | `count(DISTINCT x)`, `uniqExact` | P0 | ✅ xong, và **exact** — `uniq`, `uniqExact`, `uniqCombined`, `uniqHLL12` và `uniqTheta` đều nhận và đều trả về con số đúng, vì cardinality của bitmap không cần sketch |
| **Approx distinct** | `uniq, uniqHLL12, approx_count_distinct` | P1 | ✅ xong, và **exact**. `approx_count_distinct`/`approxCountDistinct` giờ nằm cùng danh sách với `uniq*`: một cái tên hứa xấp xỉ được trả lời chính xác là một lời hứa được giữ, không phải bị phá |
| Quantile | `quantile, quantileTDigest, median, percentile_approx` | P1 | ✅ `quantile`, `quantileExact`, `median` — và **exact**, như `count(DISTINCT)` |
| Top-K | `topK, topKWeighted, TOPN` | P1 | ✅ `topK` xong |
| Aggregate state | `-State` / `-Merge` combinator, `HLL_UNION_AGG`, `BITMAP_UNION` | P2 | |
| Array | `arrayMap/Filter/Sum/Join/Sort/Distinct`, `has`, `arrayExists` | P2 | |
| Higher-order lambda | `arrayMap(x -> x*2, arr)` | P2 | |
| String | `substring, splitByChar, like, ilike, match, position, concat` | P0 | ✅ một phần: `substring, position, concat, lower, upper, length, trim, reverse, startsWith, endsWith, splitByChar` chạy ở tầng render trên giá trị đã đọc. `like/ilike` ✅ nhưng ở `WHERE` chứ không phải ở đây — xem §13. `match`/regex thì chưa. `lower`/`upper` trong `WHERE` vẫn bị từ chối, và đó là câu trả lời đúng: các key khớp `'gb'` nằm rải rác trong từ điển chứ không gom thành một range |
| Regex | `match, extract, replaceRegexpAll` | P1 | |
| Date/time | `now, toDate, date_trunc, date_diff, date_add, toStartOfInterval, formatDateTime` | P0 | ✅ trừ `toStartOfInterval`, vốn bị từ chối *theo tên* để chỉ sang `date_trunc` — một cách viết cho một phép, để hai cách không bao giờ bất đồng về việc một tháng là gì. `date_diff`/`date_add` đếm trên lịch: thêm một tháng vào ngày 31 rơi vào ngày cuối tháng tới, không trượt sang tháng sau. **`date_trunc`/`toDate`/`toYear` trong `WHERE` giờ được trả lời**, không phải bằng cách chạy mà bằng cách *đảo lại*: mọi giá trị có tháng là tháng Một chính là mọi giá trị trong `[2024-01-01, 2024-02-01)`, và một range là phép đọc mặt phẳng bit vốn đã có. Biên tính trên *chuỗi ngày đã viết*, nên một phép viết lại đúng cho cả `DATE` lẫn `DATETIME` mà không cần schema |
| Time-series | `runningDifference, neighbor, sequenceMatch, windowFunnel, retention` | P2 | |
| Math / bit | `abs, round, floor, log, pow, bitAnd, bitShiftLeft` | P0 | ✅ xong. `round` trên `DECIMAL` là dịch dấu chấm rồi làm tròn nửa ra xa số 0 — không đi qua float, nên tiền vẫn tròn. `round`/`floor`/`ceil` trong `WHERE` cũng được đảo thành range, nhưng **ở `big-plan` chứ không ở parser**: `round(x,2) > 5` là `x >= 5.01` trên `DECIMAL(10,2)`, `x >= 6` trên `INT` và `x > 5` trên `SIGNED` — biên phụ thuộc scale, nên tầng biết scale mới tính được. `abs` thì không: nó gộp hai đoạn của một cột có dấu vào một câu trả lời |
| Type conv | `cast, toInt64, toString, parseDateTimeBestEffort` | P0 | ✅ `CAST(x AS T)`, `toInt64/toUInt64/toInt32/toFloat64/toString`. `toDateTime` vẫn từ chối: nới một số đếm ngày thành một thời điểm phải bịa ra giờ trong ngày |
| Conditional | `if, multiIf, CASE WHEN, coalesce, nullIf` | P0 | ✅ xong, cả `ifNull`. Năm cách viết vào **một** cây `Case` — dạng ngắn `CASE <expr> WHEN <val>` bị từ chối để không có cây thứ hai phải giữ đồng bộ |
| Hash | `cityHash64, xxHash64, sipHash64, murmurHash3` | P1 | |
| JSON | `JSONExtract*, json_query, ->>` | P2 | |
| URL / IP | `domain, path, IPv4NumToString` | P3 | |
| Table function | `numbers(n), s3(...), url(...), file(...), generateRandom` | P2 | |

---

## 10. Bitmap & approximate — phần đáng đầu tư nhất

Đây là chỗ bitlas có lợi thế cấu trúc, nên SQL surface phải expose nó chứ không giấu sau optimizer.

```sql
-- Doris style
SELECT bitmap_count(bitmap_and(a.uids, b.uids)) FROM ...;
SELECT bitmap_union(to_bitmap(user_id)) FROM t GROUP BY dt;
SELECT orthogonal_bitmap_union_count(uids, 1, 2, 3) FROM t;

-- ClickHouse style
SELECT bitmapCardinality(bitmapAnd(groupBitmapState(uid), ...));
SELECT bitmapSubsetInRange(bm, 100, 200);
```

**Quyết định: không thêm một hàm nào trong số này, và không thiếu gì cả.** Doris và ClickHouse cần
họ hàm này vì chúng lưu *hàng* và phải với tới một bitmap; ở đây không có cách lưu nào khác, nên
những phép chúng gọi tên chính là toán tử thường của dialect. Một cột `BITMAP` sẽ là một bitmap
nằm trong một bitmap.

Bảng ánh xạ — cũng chính là câu mà `sql_bitmap_function` nói khi một trong các tên này được gõ:

| Viết ở nơi khác | Viết ở đây | Vì sao là cùng một thứ |
|---|---|---|
| `bitmap_count(x)`, `bitmapCardinality(x)` | `count(DISTINCT x)` | cardinality của bitmap là một popcount → **exact** |
| `approx_count_distinct(x)`, `uniqHLL12(x)` | ✅ **nhận nguyên như đã viết** | cùng popcount đó; trả lời exact chứ không từ chối |
| `bsi_sum(x)` | `sum(x)` | một fold trên mặt phẳng bit |
| `bsi_range(x, a, b)` | `x BETWEEN a AND b` | BSI range, `O(bit_depth × container)` |
| `bsi_topk(x)` | `topK(n)(x)` | đúng cái ranking mà `TopN` đã mang |
| `bitmap_and(a, b)` | `a AND b` trong `WHERE` | intersect container-wise |
| `bitmap_or(a, b)` | `OR`, hoặc `IN (a, b)` | union |
| `bitmap_andnot(a, b)` | `NOT IN (SELECT _record_id FROM b …)` | difference |
| `to_bitmap(x)`, `groupBitmapState(x)` | khai `x` là cột `SET` | bitmap đã được dựng lúc ghi fact |

Thứ duy nhất trong danh sách không có cách viết ở đây là **một bitmap nằm trong một cột** và được
truyền giữa các lời gọi — đó là `sql_bitmap_type`, và nó là một quyết định chứ không phải việc chưa
làm (§15).

Chú ý: `count(DISTINCT x)` trong bitlas không cần sketch — cardinality của bitmap là exact và O(số container). Đây là điểm bán hàng, viết vào docs.

---

## 11. EXPLAIN & quan sát

```sql
EXPLAIN [PLAN | SHAPE] <statement>;                                -- done
EXPLAIN ANALYZE SELECT ...;                                        -- P2

SHOW DATABASES | TABLES [FROM db] | COLUMNS FROM t
   | CREATE TABLE t | PROCESSLIST | QUERY PROFILE;                 -- P1
DESCRIBE t;                                                        -- P0

SELECT * FROM system.tables / columns / parts / query_log
       / metrics / settings / mutations;                           -- P1

ANALYZE TABLE t;             -- statistics cho CBO                 -- P2
OPTIMIZE TABLE t FINAL;      -- compaction thủ công                -- P2
KILL QUERY WHERE query_id = '...';                                 -- P1
```

**`system.*` ✅ xong — bốn view.** Nhưng không phải "cùng một SELECT engine" như dự đoán: chúng parse
thành một `Shown`, **không** phải một query. Không plan nào sinh ra chúng, câu trả lời đã nằm sẵn trong
catalog mỗi node giữ — nên chúng nhập vào họ `SHOW` thay vì trở thành `Statement` đầu tiên có `calls`
rỗng dưới một shape gọi tên những plan không tồn tại. Đúng luật "một `Plan` variant mới là một câu trả
lời phân tán sai chứ không phải thiếu".

| View | Nội dung | Ghi chú |
|---|---|---|
| `system.tables` | mọi bảng và view, **mọi database** | Khác `SHOW TABLES` (chỉ database của request) — đó là lý do nó là variant riêng, và cột `database` là chỗ nhìn ra khác biệt |
| `system.columns` | mọi cột của mọi bảng | View cố ý vắng mặt: cột của view là cột bảng gốc dưới tên khác, đếm ở đây là đếm đôi |
| `system.databases` | mọi database | |
| `system.parts` | **mọi fragment** `(table, field, view, shard)` | **Cái duy nhất không lặp lại thứ gì.** Hai dòng mỗi field của bảng `bitmap+columnar` là đúng chứ không phải trùng: cột `view` ghi `_standard` cho bitmap và `_column` cho segment |

Chỉ nhận đúng một mệnh đề: `WHERE database = '…'`, và trên `system.columns`/`system.parts` thêm
`WHERE table = '…'`, nối bằng `AND` — vì hai cái đó **vốn đã là tham số** mà `SHOW TABLES FROM d` và
`DESCRIBE t` truyền xuống. Join, group, order, limit, `SETTINGS` đều bị từ chối (`sql_system_clause`):
câu trả lời là các dòng đọc từ catalog chứ không phải một shape trên các plan, nên lọc nó nghĩa là dựng
bộ lọc thứ hai trên rows.

`CREATE DATABASE system` bị từ chối cùng mã `sql_system_table` — một bảng trong đó không bao giờ đọc
được, vì `SELECT * FROM system.t` được đọc là system view trước khi được đọc là bảng.

`system.query_log` và `system.processlist` bị từ chối **theo tên**: một query log là một sink bền chưa
tồn tại, và một process list cần sổ đăng ký query đang chạy cũng chưa có (M3 phase 8).

---

## 12. Session & admin

```sql
SET max_threads = 8, max_memory_usage = 10000000000;
SET SESSION / GLOBAL ...;
SHOW VARIABLES LIKE 'max%';
CREATE USER u IDENTIFIED BY '...';
GRANT SELECT ON db.* TO role;
CREATE RESOURCE GROUP / WORKLOAD GROUP ...;      -- multi-tenant isolation
```

**Quyết định: không có session, nên không có `SET`.** Giới hạn được viết *lên câu lệnh* bằng mệnh
đề `SETTINGS`, cùng lý do đã từ chối `USE`: `POST /sql` trả lời một câu rồi quên sạch, nên không có
chỗ nào cho một `SET` gắn giới hạn vào. Cách viết này cũng là cách duy nhất sống sót qua một lần
reconnect, một proxy pool connection, hay một lần retry ở node khác.

```sql
SELECT count(*) FROM tx WHERE amount >= 500 SETTINGS max_execution_time = 30
```

| Nhóm setting | Ví dụ | Trạng thái |
|---|---|---|
| Giới hạn tài nguyên | `max_execution_time` (giây), `max_memory_usage` (byte), `max_result_rows` | ✅ **xong** — ba khoá này có đường đi trọn vẹn xuống `QueryLimits`/deadline của `DbRead` |
| Song song | `max_threads`, `max_block_size` | ✗ `sql_unknown_setting` — không có gì ở đây đọc chúng, nhận vào là hứa suông |
| Hành vi | `join_algorithm`, `use_index`, `allow_experimental_*` | ✗ như trên |
| Output | `output_format`, timezone | `FORMAT` đã có; timezone thì không (§8: không có timezone) |
| `SET`, `SET SESSION`, `SHOW VARIABLES` | | ✗ `sql_no_session_settings`, có chỉ sang `SETTINGS` |

Ba chỗ đáng ghi vì lệch chuẩn:

1. **`SETTINGS` đứng sau `FORMAT`**, không phải trước như ClickHouse. Một vị trí duy nhất, đọc một
   lần **sau nhánh `UNION ALL` cuối** — vì một union cho ra *một* câu trả lời, và một deadline
   theo nhánh sẽ chặn một phần mà không ai nhận riêng phần đó.
2. **Khoá lạ thì từ chối, không bỏ qua** — ngược với ClickHouse. Dialect này không có bind
   parameter, nên khoá đó do người gõ; một `max_execution_tim = 30` bị âm thầm bỏ là một câu chạy
   vô hạn trong khi tác giả tin nó có chặn, và đọc y hệt một câu có chặn mà chạy chậm.
3. **Giá trị chỉ được siết xuống, không nới lên.** Xin nhiều hơn mức operator cấu hình không phải
   câu sai — người viết query không có cách nào biết server được khởi động với gì — nên nó bị kẹp
   im lặng. Kẹp im lặng chỉ chấp nhận được vì `EXPLAIN` in ra một dòng `settings` với **con số
   thực sự được áp**; đó là chỗ người ta biết bên nào thắng.

---

## 13. Ánh xạ SQL → bitmap op (bitlas)

| SQL | Bitmap op | Chi phí |
|---|---|---|
| `WHERE f = v` | đọc row `v` của field `f` | O(container chạm) |
| `WHERE f IN (v1,v2)` | `Union(row v1, row v2)` | O(k × container) |
| `WHERE f LIKE 'x%'` | quét từ điển key của `f`, `Union` các row khớp | O(cardinality + số row khớp × container) |
| `WHERE f1 = a AND f2 = b` | `Intersect` | O(min container) |
| `WHERE f1 = a AND f2 != b` | `Difference` | như trên |
| `WHERE i BETWEEN x AND y` | BSI range | O(bit_depth × container) |
| `count(*)` | popcount | O(container), có cache |
| `count(DISTINCT f)` | số row khác rỗng | O(rows) — exact |
| `GROUP BY f` | iterate rows của `f`, intersect với filter | O(cardinality × container) |
| `GROUP BY date_trunc(u, ts)` | min/max lấy khoảng, mỗi bucket là một BSI range | O(số bucket × bit_depth × container) |
| `GROUP BY a, b, c` | đi cây: mỗi tổ hợp của các tầng trước là một lượt qua tầng sau | O(số lượt × chi phí một grouping) |
| `WHERE date_trunc(u, ts) = d` | đảo lại thành range `[d, d+1u)` | như một range viết tay |
| `WHERE round(x, k) > v` | đảo lại thành range, biên tính ở scale của field | như một range viết tay |
| `sum(i) WHERE ...` | BSI sum | O(bit_depth × container) |
| `TOP N f` | ranked cache của field | O(cache_size) |
| `LEFT SEMI JOIN` trên field chung | `Intersect` | rất rẻ |
| `WHERE ts BETWEEN ...` | chọn time view phù hợp rồi union | O(số bucket) |

**`LIKE` là ví dụ rõ nhất của nguyên tắc dưới đây.** Một cột keyed intern mỗi chuỗi phân biệt
đúng một lần, nên một pattern là *một lượt quét từ điển của một field* rồi union các bitmap khớp
— chi phí là **cardinality**, không phải số record. Engine kiểu hàng trả giá bằng số record cho
cùng câu hỏi đó. Và vì kết quả là một bitmap như mọi term khác, nó `AND` với range, `OR` với
`IN`, và `NOT` được, không cần dòng code nào riêng.

Chưa làm mà nên làm khi cardinality đủ lớn: pattern mà mọi wildcard nằm ở cuối (`'G%'`) có thể
seek B-tree của từ điển tới prefix rồi dừng ở key đầu tiên vượt qua, biến lượt quét thành một
range. Không đổi câu trả lời, chỉ đổi chi phí.

Nguyên tắc: mọi predicate quy được về Union/Intersect/Difference thì đừng để nó rơi xuống row scan. Row scan chỉ dùng lúc materialize kết quả cuối ra Arrow (đúng approach A đã chốt).

---

## 14. Lộ trình cắt scope

| Milestone | Bao gồm | Chứng minh được gì |
|---|---|---|
| **M1 — query được** | CREATE/DROP TABLE+FIELD, INSERT VALUES, `SELECT count(*) ... WHERE` với AND/OR/NOT, `DESCRIBE` | Bitmap path chạy end-to-end |
| **M1.5 — namespace** ✅ | CREATE/DROP DATABASE, tên `db.table` ở mọi statement, `SHOW DATABASES`, `SHOW TABLES FROM d`, `?database=` + `USE` ở CLI | BI tool dựng được cây bảng; nhiều team dùng chung một cluster |
| **M1.6 — view** ✅ | CREATE/DROP VIEW (lọc + chiếu trên một bảng), view lồng view, `SHOW VIEWS`, `SHOW CREATE VIEW`, `DESCRIBE v`, view trong `SHOW TABLES` | Đưa được một lát cắt hẹp của bảng cho người khác mà không copy dữ liệu |
| **M2 — hữu ích** ✅ | `GROUP BY` + count/sum, BSI range, `count(DISTINCT)`, `ORDER BY`, `LIMIT`, INSERT SELECT | Thay được một dashboard thật |
| **M2.5 — biểu thức** ✅ | Biểu thức vô hướng trong select list (số học, string, `CASE`, cast, date), `LIKE`/`ILIKE` trong `WHERE` | Không phải viết lại truy vấn ở tầng ứng dụng nữa |
| **M2.6 — chiều thời gian** ✅ | `GROUP BY date_trunc(...)`, `GROUP BY` tới 4 cột, key cạnh bucket, phép làm tròn trong `WHERE`, `LowCardinality(String)` | "Đếm theo tháng" và "theo nước, theo tháng" — hai chiều của mọi dashboard |
| **M3 — tin được** ✅ | DELETE/UPDATE theo predicate, TRUNCATE, retention, `EXPLAIN`, `system.*`, `SETTINGS` limits, KILL | Dám cho người khác dùng |
| **M4 — cạnh tranh** | JOIN (semi/anti trước), window function, ROLLUP/CUBE, bitmap function expose ra SQL, MV | So được với Doris ở use case đếm tập hợp |
| **M5 — quy mô** | Phân tán theo shard, resource group, time travel, external catalog | |

Thứ nên **làm sớm hơn thông thường** vì rẻ bất thường trên bitmap: `count(DISTINCT)` exact, SEMI/ANTI join, `SAMPLE`, `GROUPING SETS`.

**Database rẻ bất thường và đã làm rồi.** Storage khoá bằng `TableId` đã intern, không bằng tên — nên
một tầng namespace *trên* table không tốn gì dưới catalog: `FragmentKey` không đổi, layout đĩa không
đổi, file cũ đọc lên vẫn đúng (0 = `default`). Hệ quả đáng nói: **join ngang database là free**, vì
join ở đây ghép record qua *chuỗi* mà cột keyed được intern, và chuỗi thì như nhau bất kể namespace.

Thứ nên **hoãn** vì đắt bất thường: full outer join, JSON type, `MODIFY COLUMN TYPE`.

`ORDER BY` trên cột tùy ý **đã làm**, và đáng ghi lại vì sao nó từng nằm trong danh sách này: nó
không rẻ đi, chỉ là cái giá đã được nói ra thay vì giấu. Một `ORDER BY` trên projection biến
`LIMIT` từ *giới hạn số record đọc* thành *lát cắt sau khi sắp* — cùng một câu lệnh, đọc mười
record hay đọc hết, khác nhau ở một mệnh đề. Đó là thứ nên biết trước khi viết nó vào dashboard.

---

## 15. Những trần còn lại, và vì sao chúng ở đó

Không phải việc chưa làm. Mỗi cái dưới đây là một quyết định, ghi ra để không ai phải đọc code
mới biết:

**`GROUP BY` dừng ở 4 cột.** Trần thật là *số lượt đi*, kiểm ở đầu mỗi tầng — bốn là để arity của
câu trả lời còn nằm gọn trong một byte, và để một câu kể tên hai mươi cột bị từ chối ngay ở chữ
chứ không phải sau khi đã dựng xong frontier.

**`GROUP BY` chỉ nhận cột, hoặc `date_trunc` của một cột.** Hai thứ đó là hai thứ có plan.
`GROUP BY lower(country)` sẽ dán nhãn lại các giá trị mà không gộp chúng — một dòng cho mỗi giá
trị đã lưu, tất cả in ra dưới một cái tên. Kiểu `Grouping` chính là lời từ chối: nếu để lọt một
biểu thức bất kỳ vào đây thì câu hỏi "cái nào có plan" bị đẩy xuống lowering thành một danh sách
ai đó phải bảo trì.

**Select list và `GROUP BY` phải mô tả *cùng một* tập giá trị.** `SELECT ts ... GROUP BY
date_trunc('month', ts)` và `SELECT date_trunc('day', ts) ... GROUP BY date_trunc('month', ts)`
đều bị từ chối. Cả hai *trông như* đã tổng hợp mà không phải, và client không thấy được khác biệt.

**Scalar trong `WHERE` chỉ nhận hàm đảo được.** `date_trunc`, `toDate`, `toYear`, `round`,
`floor`, `ceil` — mỗi cái không giảm, nên tập giá trị cho ra một câu trả lời là *liền nhau*, tức
là một range. `lower` và `abs` thì không: câu trả lời của chúng nằm rải rác, và không có range nào
để viết lại thành.

**Một phép làm tròn so với giá trị nó không bao giờ sinh ra thì bị từ chối, không trả về rỗng.**
`date_trunc('month', ts) = '2024-01-15'` — không tháng nào bắt đầu ngày 15. Engine khác trả về
không dòng nào; ở đây từ chối, vì dialect này **không có bind parameter**, nên giá trị đó do người
gõ ra chứ không phải do thay thế — và một câu trả lời rỗng cho một lỗi gõ thì đọc y như một bảng
không có gì trong tháng đó.

**Bucket rỗng không phải một nhóm, và record không có giá trị không nằm trong bucket nào.** Cái
thứ hai là chỗ dễ hiểu sai nhất về grouping theo lịch: tổng các count là số record *có* giá trị,
không phải `count(*)`. Có một property test sinh vị từ để giữ đúng điều đó.

**`Nullable(T)` bị từ chối, và đó là nửa còn lại của việc không có null.** Vắng mặt vốn đã là cách
một cột trả lời ở đây — một record hoặc có bit được đặt hoặc không, và `SELECT *` nói điều đó bằng
cách bỏ trống ô. Khai thêm một wrapper sẽ tạo ra *cách vắng mặt thứ hai* mà không tầng nào bên dưới
phân biệt được với cách thứ nhất. Cùng một quyết định mà `IS NULL` (`sql_no_nulls`) đang tựa vào.
Từ chối ngay tại chữ `Nullable`, nên `Nullable(Decimal(10,2))` nói đúng câu đó thay vì báo lỗi cú
pháp về cái ngoặc bên trong.

**Type `BITMAP` bị từ chối vì mọi cột ở đây vốn đã là bitmap.** Một cột `SET` là một bitmap mỗi key
đã intern, một cột số là một bitmap mỗi mặt phẳng bit — nên `BITMAP` sẽ là một bitmap nằm trong một
bitmap. Cùng với nó, cả họ hàm `bitmap_*`/`bsi_*` bị từ chối **không phải vì thiếu**, mà vì mỗi cái
đã có câu trả lời dưới một cách viết khác; §10 là bảng ánh xạ, và cũng chính là câu mà
`sql_bitmap_function` nói ra. Ngoại lệ đáng ghi: `approx_count_distinct` **không** nằm trong danh
sách từ chối đó — nó đi cùng `uniq*` và được trả lời chính xác, vì một cái tên hứa xấp xỉ mà được
trả lời đúng là một lời hứa được giữ.
