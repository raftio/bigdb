# OLAP Database — Feature Cheatsheet

Cập nhật: 2026-09-06

## 1. Kiến trúc & lưu trữ

| Tính năng | Ý nghĩa | Ảnh hưởng | Ví dụ |
|---|---|---|---|
| Columnar storage | Lưu theo cột, nén 5–20× | Scan nhanh, chỉ đọc cột cần | Tất cả OLAP hiện đại |
| Tách storage/compute | Scale độc lập, compute tắt được | Chi phí, elasticity | BigQuery, Snowflake, ClickHouse Cloud |
| Shared-nothing | Compute gắn disk local | Latency thấp nhất, scale khó hơn | ClickHouse self-host, Druid |
| Open table format | Iceberg / Delta / Hudi trên object storage | Tránh lock-in, nhiều engine đọc chung | BigQuery Iceberg tables, ClickHouse, Trino, StarRocks |
| Tiered storage | Hot (SSD) → cold (S3) tự động | Giảm chi phí 60–80% cho data cũ | ClickHouse, Druid |
| Compression codec | LZ4 / ZSTD / Delta / Gorilla theo cột | Tỉ lệ nén, CPU decode | ClickHouse (chọn per-column) |

## 2. Ingest & freshness

| Tính năng | Ý nghĩa | Ảnh hưởng | Ví dụ |
|---|---|---|---|
| Streaming insert | Ghi từ Kafka/Pub/Sub, thấy data trong giây | Real-time dashboard | BQ Storage Write API, ClickHouse Kafka engine, Druid |
| Batch load | Từ file (Parquet/CSV) trên object storage | Throughput, chi phí thấp | `COPY`, `LOAD`, table function S3 |
| CDC support | Upsert / dedup theo key | Sync từ OLTP | ReplacingMergeTree, MERGE INTO, Snowflake Streams |
| Change history / time travel | Query trạng thái quá khứ, rows thay đổi | Audit, rollback, incremental ETL | BQ APPENDS/CHANGES (7 ngày), Snowflake Time Travel (90 ngày) |
| Continuous query | SQL chạy liên tục trên stream | Streaming ETL trong DB | BQ continuous queries, Materialized View ClickHouse |
| Schema evolution | Thêm/đổi cột không rebuild | DE bớt việc | Hầu hết; JSON/Dynamic type hỗ trợ tốt hơn |

## 3. Tối ưu query

| Tính năng | Ý nghĩa | Ảnh hưởng | Ví dụ |
|---|---|---|---|
| Partitioning | Cắt data theo ngày/khóa | Prune scan, giảm cost | Mọi engine |
| Clustering / sort key | Sắp xếp vật lý trong partition | Skip block, range scan nhanh | BQ clustering, CH ORDER BY, Snowflake cluster key |
| Skip index | Min-max, bloom filter, set | Lọc không cần scan | ClickHouse data skipping index |
| Materialized view | Pre-aggregate, tự refresh | Dashboard ms-level | CH incremental MV, BQ MV auto-rewrite |
| Projection | Bản sao table với sort/agg khác | Nhiều access pattern | ClickHouse, Druid rollup |
| Result / metadata cache | Trả lại query giống | Concurrency BI | BQ 24h cache, Snowflake result cache |
| Vectorized execution | SIMD, xử lý batch | CPU efficiency | ClickHouse, DuckDB, Photon |
| Approximate functions | HLL, quantile TDigest | Nhanh 10–100× cho count distinct | `approx_count_distinct`, `uniq`, `quantileTDigest` |
| Query priority / workload mgmt | Quota, queue, slot | Cách ly BI vs ETL | BQ reservation/project caps, CH settings profile |

## 4. Kiểu dữ liệu & mở rộng

| Tính năng | Ý nghĩa | Ví dụ |
|---|---|---|
| Semi-structured native | JSON / Variant / Dynamic không parse lại | BQ JSON, CH JSON/Variant, Snowflake VARIANT |
| Array / Map / Nested | Event properties, tags | ClickHouse mạnh nhất |
| Geo / vector | Geospatial, ANN search | BQ GEOGRAPHY, CH vector index |
| UDF | SQL / JS / Python / WASM | BQ Python UDF, CH executable UDF |
| ML-in-SQL | Forecast, anomaly, embedding | BQML, AI.* functions; Snowflake Cortex |
| External / federated query | Đọc DB khác không copy | BQ cross-cloud, CH `postgresql()`/`mysql()` |

## 5. Tính năng cloud (managed service)

| Tính năng | Ý nghĩa | Câu hỏi cần trả lời | Ví dụ |
|---|---|---|---|
| Serverless compute | Không cấu hình node, tự scale theo query | Có chấp nhận cold start / cost không đoán trước? | BigQuery on-demand, Snowflake serverless tasks |
| Autoscaling | Tăng/giảm compute theo tải, scale-to-zero khi idle | Idle bao nhiêu % thời gian? | ClickHouse Cloud, Snowflake multi-cluster warehouse |
| Compute isolation | Nhiều compute group dùng chung storage | BI và ETL có tranh CPU không? | Snowflake warehouses, CH Cloud compute-compute separation, BQ reservation |
| Multi-region / DR | Replicate dataset sang region khác, failover | RPO/RTO yêu cầu? | BQ cross-region replication + managed DR, Snowflake replication |
| Cross-cloud query | Query data trên cloud khác không copy | Data đang nằm ở AWS/Azure? | BQ cross-cloud connections / Omni, Snowflake cross-cloud |
| BYOC | Control plane vendor, data plane trong VPC của mình | Compliance bắt data ở tài khoản mình? | ClickHouse Cloud BYOC, Databricks |
| Managed connectors / ELT | Kéo data từ SaaS, DB, Kafka không cần code | Cần bao nhiêu nguồn? Có CDC không? | BQ Data Transfer Service, ClickPipes, Snowflake Openflow |
| Managed pipelines / orchestration | Schedule SQL, dependency, notebook trong nền tảng | Có thay được Airflow không? | BQ pipelines / Dataform, Snowflake Tasks |
| Data sharing / marketplace | Chia sẻ dataset không copy, bán/mua data | Có share với partner ngoài không? | BQ sharing (Analytics Hub) + Cloud Marketplace, Snowflake Marketplace |
| Data clean room | Query chung với partner không lộ raw data | Có use case quảng cáo / partner? | BQ clean rooms (query templates), Snowflake Clean Rooms |
| Private networking | Private Link / VPC-SC, không qua internet | Bảo mật yêu cầu gì? | GCP VPC-SC, AWS PrivateLink, Azure Private Link |
| IAM / SSO / SCIM | Đăng nhập bằng IdP công ty, sync user | Đã có Okta/Azure AD? | Cả BQ, Snowflake, CH Cloud |
| Compliance | SOC2, ISO 27001, HIPAA, PCI | Ngành có yêu cầu gì? | BQ (HIPAA cả cho Gemini), Snowflake, CH Cloud SOC2/ISO |
| CMEK / BYOK | Key mã hoá do mình quản | Audit yêu cầu key riêng? | BQ CMEK, Snowflake Tri-Secret, CH Cloud CMEK |
| Pricing model | Bytes scan / slot-hour / credit / node-hour | Workload dễ đoán hay bursty? | BQ on-demand vs edition; Snowflake credit; CH Cloud compute+storage |
| Cost control | Giới hạn bytes/query, budget alert, auto-suspend | Ai chịu trách nhiệm cost? | BQ `maximum_bytes_billed`, custom quota; Snowflake resource monitor; CH Cloud idle scaling |
| Observability & FinOps | Query log, slot/credit usage, dashboard chi phí | Có gắn vào billing export không? | INFORMATION_SCHEMA.JOBS, Snowflake ACCOUNT_USAGE, CH system tables |
| AI assistant | Sinh SQL, giải thích query, chat với data | BI user có tự query được không? | Gemini in BigQuery (conversational analytics), Snowflake Copilot |
| Managed upgrades / SLA | Vendor lo patch, upgrade; SLA 99.9–99.99% | Downtime chấp nhận bao nhiêu? | Mọi managed service; xem SLA từng tier |
| Lock-in / egress | Data ra khỏi cloud tốn bao nhiêu? | Có kế hoạch đổi cloud không? | Egress GCP/AWS ~$0.08–0.12/GB; Iceberg giảm lock-in |

## 6. Vận hành, governance, chi phí

| Tính năng | Câu hỏi cần trả lời | Ví dụ |
|---|---|---|
| Deployment | Serverless / managed / self-host? | BQ = serverless; CH = cả 3 |
| Replication / HA | RPO, RTO bao nhiêu? | BQ cross-region; CH ReplicatedMergeTree |
| RBAC + row/column security | Ai thấy cột nào? | Cả hai |
| Catalog / lineage | Có tích hợp catalog không? | Dataplex / Knowledge Catalog, Unity Catalog; CH cần tool ngoài |
| Pricing model | Theo bytes scan, slot, hay node? | BQ on-demand ~$6.25/TB scan (tham khảo); CH theo hạ tầng |
| Cost guardrail | Giới hạn bytes/query, budget alert? | BQ `maximum_bytes_billed`; CH `max_bytes_to_read` |
| Observability | Query log, slow query, slot usage | INFORMATION_SCHEMA.JOBS; system.query_log |

## 7. Checklist chọn OLAP (điểm 1–5 mỗi dòng)

| # | Tiêu chí | Câu hỏi |
|---|---|---|
| 1 | Latency | p95 cần < 1s hay < 30s? |
| 2 | Freshness | Data trễ chấp nhận: giây / phút / giờ? |
| 3 | Concurrency | Bao nhiêu user/QPS đồng thời? |
| 4 | Volume | Ingest bao nhiêu GB/ngày, giữ bao lâu? |
| 5 | Workload | Ad-hoc rộng hay dashboard cố định? |
| 6 | Ops | Team có sức vận hành cluster không? |
| 7 | Lock-in | Cần multi-cloud / open format? |
| 8 | Cost | Ngân sách/tháng và mô hình tính tiền? |
| 9 | Compliance | SOC2 / ISO / HIPAA / data residency? |
| 10 | Cloud fit | Đã ở cloud nào, cần BYOC / private link không? |

## 8. Bản đồ nhanh

| Nhu cầu | Engine phù hợp |
|---|---|
| Serverless, ad-hoc, ML-in-SQL | BigQuery, Snowflake |
| Real-time, user-facing, chi phí thấp | ClickHouse, StarRocks, Druid |
| Lakehouse open format | Databricks, Trino/Starburst, BigQuery Iceberg |
| Embedded / local analytics | DuckDB, chDB |
| Compliance chặt, data phải ở VPC mình | ClickHouse Cloud BYOC, self-host, Databricks |
