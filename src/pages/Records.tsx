import { useCallback, useEffect, useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import {
  invoke, inTauri, fmtUsdt,
  type LocalRecord, type RecordsPage, type ServerRecord,
} from "../lib";
import { Err, Skeleton, PageHead, IconAction, Empty } from "../ui";
import { IconRefresh, IconRecords, IconArrowLeft, IconArrowRight, IconStore, IconChip } from "../icons";
import { errText } from "../errors";

type Role = "provider" | "consumer";
const fmtTime = (secs: number) => (secs ? new Date(secs * 1000).toLocaleString() : "—");

export function Records() {
  const { t } = useTranslation();
  const [role, setRole] = useState<Role>("provider");
  const [page, setPage] = useState(1);
  const [data, setData] = useState<RecordsPage | null>(null);
  const [loading, setLoading] = useState(inTauri);
  const [err, setErr] = useState("");
  const [refreshing, setRefreshing] = useState(false);

  const load = useCallback((r: Role, p: number, manual = false) => {
    if (!inTauri) return;
    if (manual) setRefreshing(true); else setLoading(true);
    invoke<RecordsPage>("records_query", { role: r, page: p })
      .then((d) => { setData(d); setErr(""); })
      .catch((e) => setErr(errText(e)))
      .finally(() => { setLoading(false); setRefreshing(false); });
  }, []);
  useEffect(() => load(role, page), [role, page, load]);

  const statusBadge = (s: number) => {
    const map: Record<number, [string, string]> = {
      1: ["warn", "statusRunning"], 2: ["on", "statusDone"], 3: ["err", "statusFailed"],
      4: ["warn", "statusDisputed"], 0: ["off", "statusPending"],
    };
    const [cls, key] = map[s] ?? map[0];
    return <span className={`pill ${cls}`}>{t(`records.${key}`)}</span>;
  };

  const useLocal = !!data?.server_error || (data?.records?.length ?? 0) === 0;
  const serverRows: ServerRecord[] = data?.records ?? [];
  const localRows: LocalRecord[] = data?.local.records ?? [];
  const rowsShown = useLocal ? localRows.length : serverRows.length;
  const hasNext = useLocal
    ? page * (data?.per_page ?? 50) < (data?.local.total ?? 0)
    : rowsShown === (data?.per_page ?? 50);

  const roles: { id: Role; icon: ReactNode; label: string }[] = [
    { id: "provider", icon: <IconStore />, label: t("records.tabProvider") },
    { id: "consumer", icon: <IconChip />, label: t("records.tabConsumer") },
  ];

  return (
    <div>
      <PageHead
        title={t("records.title")}
        sub={t("records.sub")}
        actions={
          <>
            <div className="segmented sm">
              {roles.map((r) => (
                <button key={r.id} className={role === r.id ? "active" : ""} onClick={() => { setRole(r.id); setPage(1); }}>
                  {r.icon}{r.label}
                </button>
              ))}
            </div>
            <IconAction
              icon={<IconRefresh />}
              label={t("records.refresh")}
              onClick={() => load(role, page, true)}
              disabled={!inTauri || refreshing}
              spinning={refreshing}
            />
          </>
        }
      />

      <div className="card">

        {data?.server_error && <p className="card-desc">{t("records.serverUnavailable", { msg: data.server_error })}</p>}
        {err && <Err>{err}</Err>}

        {loading ? (
          <div className="table-wrap">
            <table className="tbl">
              <thead>
                <tr>
                  <th>{t("records.time")}</th>
                  <th>{t("records.model")}</th>
                  <th>{t("records.tokens")}</th>
                  <th>{t("records.amount")}</th>
                  <th>{t("records.fee")}</th>
                  <th>{t("records.status")}</th>
                </tr>
              </thead>
              <tbody>
                {Array.from({ length: 6 }, (_, i) => (
                  <tr key={i}>
                    <td><Skeleton w={130} h={13} /></td>
                    <td><Skeleton w={90} h={13} /></td>
                    <td><Skeleton w={70} h={13} /></td>
                    <td><Skeleton w={60} h={13} /></td>
                    <td><Skeleton w={60} h={13} /></td>
                    <td><Skeleton w={64} h={18} r={999} /></td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        ) : rowsShown === 0 ? (
          <Empty
            icon={<IconRecords />}
            title={t("records.empty")}
            desc={t(role === "provider" ? "records.emptyProviderDesc" : "records.emptyConsumerDesc")}
          />
        ) : (
          <div className="table-wrap">
            <table className="tbl">
              <thead>
                <tr>
                  <th>{t("records.time")}</th>
                  <th>{t("records.model")}</th>
                  <th>{t("records.tokens")}</th>
                  <th>{t("records.amount")}</th>
                  <th>{t("records.fee")}</th>
                  <th>{t("records.status")}</th>
                </tr>
              </thead>
              <tbody>
                {!useLocal && serverRows.map((r) => (
                  <tr key={r.task_id}>
                    <td className="tabular">{fmtTime(r.created_ts)}</td>
                    <td className="mono">{r.model}</td>
                    <td className="mono tabular">{r.in_tokens}<span className="faint"> / </span>{r.out_tokens}</td>
                    <td className="mono tabular">{fmtUsdt(role === "provider" ? r.provider_income : r.amount_usdt)}</td>
                    <td className="mono tabular">
                      {fmtUsdt(r.platform_fee)}
                      {r.amount_usdt > 0 && (
                        <span className="faint"> ({Math.round((r.platform_fee / r.amount_usdt) * 100)}%)</span>
                      )}
                    </td>
                    <td>{statusBadge(r.status)}</td>
                  </tr>
                ))}
                {useLocal && localRows.map((r) => (
                  <tr key={r.task_id}>
                    <td className="tabular">{fmtTime(r.ts)} <span className="pill off plain tiny">{t("records.localBadge")}</span></td>
                    <td className="mono">{r.model || "—"}</td>
                    <td className="mono tabular">{r.in_tokens}<span className="faint"> / </span>{r.out_tokens}</td>
                    <td className="mono tabular">{fmtUsdt(r.amount_usdt)}</td>
                    <td className="faint">—</td>
                    <td><span className="pill off">{r.status}</span></td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}

        {(page > 1 || hasNext) && (
          <div className="pager">
            <button className="btn sm ghost" onClick={() => setPage((p) => Math.max(1, p - 1))} disabled={page <= 1}><IconArrowLeft />{t("records.prev")}</button>
            <span className="pager-n">{t("records.pageN", { page })}</span>
            <button className="btn sm ghost" onClick={() => setPage((p) => p + 1)} disabled={!hasNext}>{t("records.next")}<IconArrowRight /></button>
          </div>
        )}
      </div>
    </div>
  );
}
