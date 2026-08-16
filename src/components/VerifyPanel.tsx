/**
 * Model verification, as the seller sees it.
 *
 * # What this is showing them
 *
 * The platform buys a handful of short completions from one of their lanes,
 * through the ordinary buyer path, and checks whether the replies behave like
 * the model the lane is sold as. It runs entirely on the server: nothing here
 * probes anything, and nothing here decides anything. This component starts a
 * run and reports what came back.
 *
 * That division is not an implementation detail to be hidden — it is the reason
 * the result is worth anything. A check the app performed on itself would be a
 * check whoever modified the app removes.
 *
 * # Why it says the sampling will continue
 *
 * Because it will, and because a seller who learns that from a support reply
 * after their lane was pulled has been treated badly. It is also the honest
 * framing of what passing means: not "cleared", but "cleared for now, and we
 * will keep checking without telling you when". The deterrent only works if
 * people know it exists — what stays unannounced is the timing, never the fact.
 */
import { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { invoke } from "../lib";
import { errText } from "../errors";
import { IconCheck, IconAlert, IconInfo, IconShield } from "../icons";

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

/** Drive one verification run. */
export function useVerification(provider: string, model: string) {
  const [job, setJob] = useState<VerifyJob | null>(null);
  const [err, setErr] = useState("");
  const [starting, setStarting] = useState(false);
  const timer = useRef<number | undefined>(undefined);

  const stop = useCallback(() => {
    if (timer.current !== undefined) {
      clearInterval(timer.current);
      timer.current = undefined;
    }
  }, []);

  // Never leave a poll running behind a closed dialog.
  useEffect(() => stop, [stop]);

  const poll = useCallback(
    async (jobId: string) => {
      try {
        const j = await invoke<VerifyJob>("lane_verification_job", { jobId });
        setJob(j);
        if (j.state !== "running") stop();
      } catch (e) {
        // A failed poll is not a failed run. The run is on the server; the next
        // tick will very likely find it. Only a run that reports its own
        // failure ends this.
        setErr(errText(e));
      }
    },
    [stop],
  );

  const start = useCallback(async () => {
    stop();
    setErr("");
    setJob(null);
    setStarting(true);
    try {
      const { job_id } = await invoke<{ job_id: string }>("start_lane_verification", {
        provider,
        model,
      });
      setJob({
        job_id,
        state: "running",
        progress: 0,
        step: "",
        outcome: "",
        error: "",
        model,
        provider,
      });
      timer.current = window.setInterval(() => void poll(job_id), POLL_MS);
      void poll(job_id);
    } catch (e) {
      setErr(errText(e));
    } finally {
      setStarting(false);
    }
  }, [provider, model, poll, stop]);

  const running = starting || job?.state === "running";
  return { job, err, running, start };
}

/** Verification for one lane: run it, watch it, read the verdict. */
export function VerifyPanel({
  provider,
  model,
  verdict,
  onOutcome,
}: {
  provider: string;
  model: string;
  /** The standing already on record, shown before a new run is started. */
  verdict?: LaneVerdict;
  onOutcome?: (outcome: string) => void;
}) {
  const { t } = useTranslation();
  const { job, err, running, start } = useVerification(provider, model);

  // Report terminal outcomes upward so a caller gating on one (the sell switch)
  // does not have to re-derive it from the job shape.
  useEffect(() => {
    if (job?.state === "done" && job.outcome) onOutcome?.(job.outcome);
  }, [job?.state, job?.outcome]); // eslint-disable-line react-hooks/exhaustive-deps

  // Prefer the live run; fall back to whatever was already on record.
  //
  // An expired pass is not a pass. The gateway stops routing buyers to a lane
  // the moment its verdict lapses, so a page that kept rendering 「验证通过」
  // would be telling the seller their lane is fine while it quietly earns
  // nothing — and, worse, showing "valid until <a date in the past>" directly
  // underneath. Reading it as unverified matches what the market is actually
  // doing and puts the re-verify button in front of them.
  const stale =
    verdict?.status === "pass" &&
    !!verdict.expires_at &&
    new Date(verdict.expires_at).getTime() <= Date.now();
  const recorded = stale ? "" : verdict?.status ?? "";
  const outcome = job?.state === "done" ? job.outcome : recorded;

  return (
    <div className="verify-panel">
      {running ? (
        <VerifyProgress job={job} />
      ) : outcome ? (
        <VerifyVerdict outcome={outcome} verdict={verdict} live={job?.state === "done"} />
      ) : (
        <div className="callout compact">
          <IconInfo />
          <span>{t(stale ? "publish.verify.expired" : "publish.verify.never")}</span>
        </div>
      )}

      {/* Said before the button, not after the result. Somebody deciding
          whether to put their subscription on the market is owed the whole
          arrangement up front — including the part that keeps happening after
          this dialog is closed. */}
      <div className="callout compact">
        <IconShield />
        <span>{t("publish.verify.samplingNote")}</span>
      </div>
      <div className="callout compact">
        <IconInfo />
        <span>{t("publish.verify.costNote")}</span>
      </div>

      <div className="btn-row" style={{ marginTop: "var(--s4)" }}>
        <button className="btn" onClick={() => void start()} disabled={running || !model}>
          {running
            ? t("publish.verify.running")
            : outcome
              ? t("publish.verify.rerun")
              : t("publish.verify.run")}
        </button>
      </div>

      {/* Promised by the reject copy, so it has to exist. A seller told their
          lane is not the model they are selling is owed the probes that said
          so — both because they may be right and we may be wrong, and because
          "we ran some checks and you failed" is not a thing anyone can act on. */}
      {outcome && !running && <Evidence provider={provider} model={model} />}

      {err && <div className="callout warn"><IconAlert /><span>{err}</span></div>}
    </div>
  );
}

/** The probes behind a verdict, on demand.
 *
 *  Collapsed by default and fetched only when opened: a report is tens of
 *  kilobytes, and the panel around it is on a poll. */
function Evidence({ provider, model }: { provider: string; model: string }) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const [report, setReport] = useState<VerifyReport | null>(null);
  const [err, setErr] = useState("");

  useEffect(() => {
    if (!open || report) return;
    invoke<VerifyReport>("lane_verification_report", { provider, model })
      .then(setReport)
      .catch((e) => setErr(errText(e)));
  }, [open, report, provider, model]);

  if (!open) {
    return (
      <button className="btn subtle" onClick={() => setOpen(true)}>
        {t("publish.verify.evidence.open")}
      </button>
    );
  }
  return (
    <div className="verify-evidence">
      {err && <div className="hint">{err}</div>}
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
                <span className="verify-probe-note">{r.summary}</span>
              </li>
            ))}
          </ul>
        </>
      )}
    </div>
  );
}

/** The bar, and what it is doing right now.
 *
 *  The step name matters more than the percentage here: a run spends most of
 *  its time inside two of its steps, so a bar alone looks stuck for twenty
 *  seconds at a stretch while the number next to it is still moving. */
function VerifyProgress({ job }: { job: VerifyJob | null }) {
  const { t } = useTranslation();
  const pct = Math.max(4, job?.progress ?? 0);
  const step = job?.step ?? "";
  return (
    <div className="verify-progress">
      <div className="verify-bar" aria-hidden>
        <div className="verify-bar-fill" style={{ width: `${pct}%` }} />
      </div>
      <div className="sub">
        {step
          // The registry ids are a fixed, small set, so a missing translation
          // falls back to the id rather than to an empty line.
          ? t(`publish.verify.step.${step}`, { defaultValue: step })
          : t("publish.verify.starting")}
      </div>
    </div>
  );
}

/** The verdict, in terms that say what happens next rather than what scored. */
function VerifyVerdict({
  outcome,
  verdict,
  live,
}: {
  outcome: string;
  verdict?: LaneVerdict;
  live: boolean;
}) {
  const { t } = useTranslation();
  const tone = outcome === "pass" ? "" : outcome === "reject" ? "warn" : "warn";
  const Icon = outcome === "pass" ? IconCheck : IconAlert;
  return (
    <div className={`callout ${tone}`} style={{ display: "block" }}>
      <div className="verify-verdict-head">
        <Icon />
        <strong>{t(`publish.verify.outcome.${outcome}`, { defaultValue: outcome })}</strong>
      </div>
      <p className="sub">{t(`publish.verify.outcomeBody.${outcome}`, { defaultValue: "" })}</p>
      {verdict && !live && verdict.samples > 0 && (
        // The sampling record, which is the part that accumulates into
        // something worth showing a buyer. Displayed on purpose: it is the
        // seller's own evidence that they have been checked repeatedly and
        // passed, which is what the badge is actually claiming.
        <p className="sub">
          {t("publish.verify.sampleRecord", {
            samples: verdict.samples,
            fails: verdict.sample_fails,
            unclear: verdict.sample_unclear,
          })}
        </p>
      )}
      {verdict?.expires_at && outcome === "pass" && (
        <p className="sub">
          {t("publish.verify.expires", {
            date: new Date(verdict.expires_at).toLocaleDateString(),
          })}
        </p>
      )}
    </div>
  );
}
