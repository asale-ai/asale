/**
 * Model verification, as the seller sees it.
 *
 * # What this is showing them
 *
 * The platform buys a handful of short completions from one of their lanes,
 * through the ordinary buyer path, and checks whether the replies behave like
 * the model the lane is sold as. It runs entirely on the server: nothing here
 * probes anything, and nothing here decides anything. This component starts the
 * runs and reports what came back.
 *
 * That division is not an implementation detail to be hidden — it is the reason
 * the result is worth anything. A check the app performed on itself would be a
 * check whoever modified the app removes.
 *
 * # Why an account is the unit
 *
 * A verdict belongs to one lane — one `(device, provider, model)` — and the
 * market routes buyers away from every lane that has not got one. A seller,
 * though, connects an *account*, and it offers a dozen models. Verifying them
 * one dialog at a time meant the models nobody remembered to pick stayed dark
 * indefinitely, so the choice here is a checklist with everything already
 * ticked, and the runs are scheduled a few at a time.
 *
 * # Why the copy is short
 *
 * The sampling continues after a pass, and the seller is told so — once. It
 * used to be said in three stacked callouts around a single dropdown, which is
 * how a two-line arrangement becomes a wall that gets scrolled past. What has to
 * be unannounced is the *timing* of the sampling, never the fact of it, and one
 * sentence carries the fact perfectly well.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { invoke } from "../lib";
import { errText } from "../errors";
import { IconAlert, IconChevronDown } from "../icons";

/** A run in flight, or the one that just finished. */
export interface VerifyJob {
  job_id: string;
  /** running | done | failed */
  state: string;
  /** 0–100. An upper bound: several probes skip themselves. */
  progress: number;
  /** `llm-verify` registry id of the current step. Stable across releases,
   *  which is what lets us translate it into four languages the report itself
   *  is not written in. */
  step: string;
  /** pass | watch | reject — only once `state` is `done`. */
  outcome: string;
  error: string;
  model: string;
  provider: string;
}

/** One lane's standing, from `/me/lanes/verification`. */
export interface LaneVerdict {
  device_id: string;
  provider: string;
  model: string;
  status: string;
  score: number;
  confidence: number;
  authenticity: string;
  verified_at: string | null;
  expires_at: string | null;
  samples: number;
  sample_fails: number;
  /** Samples that could not conclude. Reported apart from the failures because
   *  "34 samples, 0 failures" means something very different depending on
   *  whether those 34 were clean or merely unreadable. */
  sample_unclear: number;
  /** The most recent sample's own verdict, which can differ from `status`: an
   *  inconclusive sample does not overturn an unexpired pass. */
  last_sample_status: string;
}

/** One probe's result, as `llm-verify` writes it into the report. */
export interface ProbeResult {
  id: string;
  label: string;
  group: string;
  /** pass | warn | fail | skip | error */
  status: string;
  summary: string;
}

/** The stored report behind a verdict. Only the fields the evidence view
 *  reads — the full document has a dozen more the client has no use for. */
export interface VerifyReport {
  results: ProbeResult[];
  verdict: { score: number; confidence: number; authenticity: string };
  request_count: number;
  /** A string, not a number. The seed is a u64 and routinely exceeds 2^53,
   *  where JSON numbers stop being exact in JavaScript — read as a number it
   *  renders as a value that reproduces nothing, under a caption promising it
   *  reproduces the run. */
  seed: string;
  depth: string;
}

export interface VerifyOverview {
  enabled: boolean;
  /** Whether a failing verdict currently costs the lane anything. False during
   *  a measure-only rollout — and the UI must not threaten a consequence that
   *  is not happening. */
  enforced: boolean;
  ttl_days: number;
  lanes: LaneVerdict[];
}

/** How often to ask the server where a run has got to.
 *
 *  A run is tens of seconds of upstream calls, so this is about the progress
 *  bar moving smoothly rather than about latency. Polling is enough — the run
 *  lives on the server and does not care whether anybody is watching, which is
 *  also what lets the window be closed and reopened mid-run. */
const POLL_MS = 1500;

/** How many lanes are verified at once.
 *
 *  Every probe is a real purchase from the seller's own subscription, so this is
 *  a choice about somebody else's account rather than about our own throughput.
 *  One at a time is the kindest and, on an account offering a dozen models, slow
 *  enough that people close the dialog and never learn the answer. Three is
 *  comfortably under the five concurrent requests a lane declares by default, so
 *  a batch never becomes the reason its own probes are queued behind each
 *  other. */
const BATCH_WIDTH = 3;

/** One lane's slot in a batch run. */
export interface BatchRow {
  model: string;
  state: "idle" | "pending" | "running" | "done" | "failed";
  /** pass | watch | reject, once `state` is `done`. */
  outcome: string;
  /** Why the run could not produce a verdict, once `state` is `failed`. */
  error: string;
  /** The `llm-verify` step currently running, for the row's own progress line. */
  step: string;
}

/** Verify several of one account's lanes in one go.
 *
 *  A seller with a dozen models has a dozen lanes, each verified separately, and
 *  the market routes buyers away from every one that has not been. Offering the
 *  check one model at a time made the common case — "I just connected this
 *  account, clear all of it" — a dozen dialogs, and the models nobody thought to
 *  pick stayed dark for as long as it took somebody to notice.
 *
 *  Each lane is still its own server-side run: this schedules them and collects
 *  the answers, and decides nothing. */
export function useBatchVerification(provider: string) {
  const [rows, setRows] = useState<Record<string, BatchRow>>({});
  const [running, setRunning] = useState(false);
  const [err, setErr] = useState("");
  // Bumped on unmount and on every fresh start, so results from an abandoned
  // batch cannot land in the one the user is now watching.
  const epoch = useRef(0);

  useEffect(() => () => { epoch.current += 1; }, []);

  const patch = useCallback((mine: number, model: string, next: Partial<BatchRow>) => {
    if (epoch.current !== mine) return;
    setRows((r) => ({ ...r, [model]: { ...r[model], model, ...next } as BatchRow }));
  }, []);

  /** One lane, start to terminal state. Never throws: a lane that fails is a
   *  row that says so, not a batch that stops. */
  const one = useCallback(
    async (mine: number, model: string) => {
      patch(mine, model, { state: "running", outcome: "", error: "", step: "" });
      let jobId = "";
      try {
        const started = await invoke<{ job_id: string }>("start_lane_verification", { provider, model });
        jobId = started.job_id;
      } catch (e) {
        patch(mine, model, { state: "failed", error: errText(e) });
        return;
      }
      // Polling rather than a socket, for the same reason the single-lane panel
      // polls: the run is the server's and survives this dialog.
      for (;;) {
        await new Promise((r) => setTimeout(r, POLL_MS));
        if (epoch.current !== mine) return;
        let job: VerifyJob;
        try {
          job = await invoke<VerifyJob>("lane_verification_job", { jobId });
        } catch {
          // A dropped poll is not a dropped run — try again on the next tick.
          continue;
        }
        if (job.state === "running") {
          patch(mine, model, { step: job.step });
          continue;
        }
        patch(mine, model, {
          state: job.state === "done" ? "done" : "failed",
          outcome: job.outcome,
          error: job.error,
          step: "",
        });
        return;
      }
    },
    [provider, patch],
  );

  const start = useCallback(
    async (models: string[]) => {
      if (models.length === 0) return;
      const mine = (epoch.current += 1);
      setErr("");
      setRows(Object.fromEntries(
        models.map((m) => [m, { model: m, state: "pending", outcome: "", error: "", step: "" } as BatchRow]),
      ));
      setRunning(true);
      try {
        let next = 0;
        const worker = async () => {
          for (;;) {
            const i = next++;
            if (i >= models.length || epoch.current !== mine) return;
            await one(mine, models[i]);
          }
        };
        await Promise.all(
          Array.from({ length: Math.min(BATCH_WIDTH, models.length) }, worker),
        );
      } catch (e) {
        setErr(errText(e));
      } finally {
        if (epoch.current === mine) setRunning(false);
      }
    },
    [one],
  );

  return { rows, running, err, start };
}

/** Pick which of an account's models an action applies to.
 *
 *  Everything is ticked when it opens, which is the answer almost everybody
 *  wants: an account's lanes are verified — or tested — as a set, and the seller
 *  who only wants one of them is the exception. Ticking twelve boxes to express
 *  the common case is a worse default than unticking one to express the rare
 *  one. */
export function ModelChecklist({
  models, value, onChange, disabled, label, note, detail, expandable, unavailable,
}: {
  models: string[];
  value: string[];
  onChange: (next: string[]) => void;
  disabled?: boolean;
  label: string;
  /** Per-model right-hand annotation (a standing, a result, a spinner). */
  note?: (model: string) => React.ReactNode;
  /** What the row expands into. Rendered only while that row is open, so the
   *  caller can put a network fetch behind it. */
  detail?: (model: string) => React.ReactNode;
  /** Whether this model has anything to expand *into*. Without it a row would
   *  offer a disclosure that opens onto "not found": the evidence behind a
   *  verdict only exists once there is a verdict. */
  expandable?: (model: string) => boolean;
  /** Rows that cannot be acted on, and are shown anyway.
   *
   *  Listing them is the point. A model missing from the list reads as a model
   *  the account does not have, which sends the seller to look for it in the
   *  wrong place; a model listed, greyed and annotated says what it is waiting
   *  for. The caller is expected to put the reason in `note`. */
  unavailable?: (model: string) => boolean;
}) {
  const { t } = useTranslation();
  const pickable = models.filter((m) => !unavailable?.(m));
  const all = pickable.length > 0 && pickable.every((m) => value.includes(m));
  const toggle = (m: string) =>
    onChange(value.includes(m) ? value.filter((x) => x !== m) : [...value, m]);

  /* One row open at a time.
   *
   * Not a stylistic choice: each report is tens of kilobytes fetched on open,
   * and a list of a dozen models that can all be open at once is a dozen
   * fetches for a comparison nobody is making. Opening one closes the last. */
  const [open, setOpen] = useState("");
  // A model that leaves the list must not leave its panel open over the row
  // that inherits its position.
  useEffect(() => {
    if (open && !models.includes(open)) setOpen("");
  }, [models.join(" "), open]); // eslint-disable-line react-hooks/exhaustive-deps

  return (
    <div className="field">
      <div className="check-head">
        <label>{label}</label>
        <button
          type="button"
          className="lane-resume"
          onClick={() => onChange(all ? [] : pickable)}
          disabled={disabled || pickable.length === 0}
        >
          {t(all ? "publish.verify.selectNone" : "publish.verify.selectAll")}
        </button>
      </div>
      {/* An open row drops the list's own height cap: a panel of probes inside
          a 20rem scroller, inside the dialog's scroller, is two scrollbars for
          one page of text. */}
      <div className={`model-checklist${open ? " expanded" : ""}`}>
        {models.length === 0 && (
          <div className="mc-item"><div className="mc-row"><span className="mc-name faint">{t("publish.verify.noModels")}</span></div></div>
        )}
        {models.map((m) => {
          const shown = open === m;
          // The disclosure lives beside the checkbox rather than inside its
          // label: nested in one, every click on the chevron would also tick
          // the box it was trying not to touch.
          const canOpen = !!detail && (!expandable || expandable(m));
          const off = !!unavailable?.(m);
          return (
            <div key={m} className={`mc-item${shown ? " open" : ""}${off ? " unavailable" : ""}`}>
              <div className="mc-row">
                <label className={`mc-pick${value.includes(m) && !off ? " on" : ""}`}>
                  <input
                    type="checkbox"
                    checked={value.includes(m) && !off}
                    onChange={() => toggle(m)}
                    disabled={disabled || off}
                  />
                  <span className="mc-name mono">{m}</span>
                </label>
                {note && <span className="mc-note">{note(m)}</span>}
                {detail && (canOpen ? (
                  <button
                    type="button"
                    className={`mc-toggle${shown ? " on" : ""}`}
                    aria-expanded={shown}
                    title={t(shown ? "publish.verify.evidence.close" : "publish.verify.evidence.open")}
                    aria-label={t(shown ? "publish.verify.evidence.close" : "publish.verify.evidence.open")}
                    onClick={() => setOpen(shown ? "" : m)}
                  >
                    <IconChevronDown />
                  </button>
                ) : (
                  // Holds the column so the names, the standings and the
                  // chevrons of the rows that have one all stay in line.
                  <span className="mc-toggle-gap" aria-hidden />
                ))}
              </div>
              {shown && detail && <div className="mc-detail">{detail(m)}</div>}
            </div>
          );
        })}
      </div>
    </div>
  );
}

/** Verify a whole account's lanes: pick them, run them, read the answers.
 *
 *  Deliberately short on prose. The dialog around it says, once, what a
 *  verification is and what it costs; a panel that repeated it in three callouts
 *  turned a two-line explanation into a wall nobody reads. */
export function BatchVerifyPanel({
  provider, models, verdicts,
}: {
  provider: string;
  models: string[];
  /** Standings already on record, shown before anything is run. */
  verdicts: LaneVerdict[];
}) {
  const { t } = useTranslation();
  const { rows, running, err, start } = useBatchVerification(provider);
  const [picked, setPicked] = useState<string[]>(models);

  // Follow the model list: an account whose lanes arrive a moment after the
  // dialog opens should still start out fully ticked.
  useEffect(() => { setPicked(models); }, [models.join(" ")]); // eslint-disable-line react-hooks/exhaustive-deps

  const done = Object.values(rows).filter((r) => r.state === "done" || r.state === "failed").length;
  const total = Object.keys(rows).length;
  // Distinct failure sentences, in the order they first appeared.
  const reasons = useMemo(
    () => [...new Set(
      Object.values(rows).filter((r) => r.state === "failed" && r.error).map((r) => r.error),
    )],
    [rows],
  );

  const standing = (model: string): React.ReactNode => {
    const r = rows[model];
    if (r?.state === "running") return <span className="faint">{r.step ? t(`publish.verify.step.${r.step}`, { defaultValue: r.step }) : t("publish.verify.rowRunning")}</span>;
    if (r?.state === "pending") return <span className="faint">{t("publish.verify.rowPending")}</span>;
    // A run that never reached the lane is not a verdict, and the server no
    // longer files one — so it is reported as what it is rather than folded in
    // with the inconclusive results.
    if (r?.state === "failed") return <span className="bad" title={r.error}>{t("publish.verify.rowUnreachable")}</span>;
    const outcome = r?.outcome || recordedStatus(verdicts, provider, model);
    if (!outcome) return <span className="faint">{t("publish.verify.outcome.pending")}</span>;
    return (
      <span className={outcome === "pass" ? "good" : outcome === "reject" ? "bad" : "warn"}>
        {t(`publish.verify.outcome.${outcome}`, { defaultValue: outcome })}
      </span>
    );
  };

  /** Whether a lane has evidence to show.
   *
   *  A report is written with the verdict, so "there is a verdict" is the
   *  question — including an expired one, whose probes are still the last thing
   *  anybody learned about the lane. A run that failed to reach the lane
   *  produced no report and gets no disclosure. */
  const hasEvidence = (model: string) =>
    rows[model]?.state === "done"
    || verdicts.some((v) => v.provider === provider && v.model === model);

  return (
    <>
      <ModelChecklist
        models={models}
        value={picked}
        onChange={setPicked}
        disabled={running}
        label={t("publish.verify.pickModels")}
        note={standing}
        expandable={hasEvidence}
        // Keyed on the model so switching rows remounts rather than showing
        // the previous lane's probes under the new lane's name.
        detail={(m) => <Evidence key={m} provider={provider} model={m} />}
      />

      <div className="btn-row" style={{ marginTop: "var(--s4)" }}>
        <button
          className="btn"
          onClick={() => void start(picked)}
          disabled={running || picked.length === 0}
        >
          {running
            ? t("publish.verify.runningN", { done, total })
            : t("publish.verify.runN", { n: picked.length })}
        </button>
      </div>

      {/* Why the failed lanes failed, once rather than once per row. The reason
          is almost always the same one for all of them — the account is not
          actually on the market — and a checklist column is too narrow to say
          anything a seller could act on. */}
      {reasons.map((r) => (
        <div key={r} className="callout warn compact"><IconAlert /><span>{r}</span></div>
      ))}

      {err && <div className="callout warn"><IconAlert /><span>{err}</span></div>}
    </>
  );
}

/** The standing already on record for one lane, ignoring an expired pass —
 *  see [`VerifyPanel`] for why a lapsed verdict must not read as a pass.
 *
 *  Exported because the sell page's price chart has to ask the same question:
 *  under an enforcing gate a lane with no verdict is not selling, whatever its
 *  own switch says, and the chart used to draw it green. */
export function recordedStatus(verdicts: LaneVerdict[], provider: string, model: string): string {
  const v = verdicts.find((x) => x.provider === provider && x.model === model);
  if (!v) return "";
  const stale = v.status === "pass" && !!v.expires_at && new Date(v.expires_at).getTime() <= Date.now();
  return stale ? "" : v.status;
}

/** The probes behind one lane's verdict.
 *
 *  Mounted only while its row is expanded, and it fetches on mount: a report is
 *  tens of kilobytes, and the panel around it is on a poll. */
function Evidence({ provider, model }: { provider: string; model: string }) {
  const { t } = useTranslation();
  const [report, setReport] = useState<VerifyReport | null>(null);
  const [err, setErr] = useState("");

  useEffect(() => {
    let live = true;
    invoke<VerifyReport>("lane_verification_report", { provider, model })
      .then((r) => { if (live) setReport(r); })
      .catch((e) => { if (live) setErr(errText(e)); });
    return () => { live = false; };
  }, [provider, model]);

  return (
    <div className="verify-evidence">
      {err && <div className="hint">{err}</div>}
      {!report && !err && <div className="hint">{t("publish.verify.evidence.loading")}</div>}
      {report && (
        <>
          <div className="sub">
            {t("publish.verify.evidence.summary", {
              requests: report.request_count,
              depth: report.depth,
              seed: report.seed,
            })}
          </div>
          <ul className="verify-probes">
            {report.results.map((r) => (
              <li key={r.id} className={`verify-probe ${r.status}`}>
                <span className="verify-probe-status" aria-hidden>
                  {r.status === "pass" ? "\u2713" : r.status === "fail" ? "\u2717" : r.status === "warn" ? "!" : "\u2013"}
                </span>
                {/* The label from the report is English or Chinese only; the id
                    is stable, so the four-language name comes from our own
                    table and the report's own prose is the fallback. */}
                <span className="verify-probe-name">
                  {t(`publish.verify.step.${r.id}`, { defaultValue: r.label })}
                </span>
                <span className="verify-probe-note" title={r.summary}>{r.summary}</span>
              </li>
            ))}
          </ul>
        </>
      )}
    </div>
  );
}
