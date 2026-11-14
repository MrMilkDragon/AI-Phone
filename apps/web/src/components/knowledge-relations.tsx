"use client";

import {
  KnowledgeDocumentSchema, KnowledgeLinkRequestSchema, KnowledgeListSchema, WorkbenchRequestSchema,
} from "@ai-phone/protocol/ai_phone/v1/workbench_pb";
import type {MessageShape} from "@bufbuild/protobuf";
import {ArrowUpRight, Check, Link2, Search, X} from "lucide-react";
import {useEffect, useId, useRef, useState} from "react";
import {workbench} from "@/lib/workbench-client";
import {useI18n} from "./locale-provider";
import {Button} from "./ui/button";
import {LoadingOrb} from "./ui/loading-orb";

type Doc = MessageShape<typeof KnowledgeDocumentSchema>;

export function KnowledgeRelations({doc, disabled, onChanged, onSelect}: {
  doc: Pick<Doc, "id" | "revision" | "relatedIds">;
  disabled: boolean;
  onChanged: (value: Doc) => void;
  onSelect: (id: string) => void;
}) {
  const {t} = useI18n();
  const picker = useId();
  const trigger = useRef<HTMLButtonElement>(null);
  const [related, setRelated] = useState<Doc[]>([]);
  const [options, setOptions] = useState<Doc[]>([]);
  const [loaded, setLoaded] = useState(false);
  const [query, setQuery] = useState("");
  const [picking, setPicking] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");

  useEffect(() => {
    let live = true;
    void workbench("knowledge-related", WorkbenchRequestSchema, {id: doc.id}, KnowledgeListSchema)
      .then(value => { if (live) setRelated(value.documents); })
      .catch(error => { if (live) setError(error instanceof Error ? error.message : "关联失败"); });
    return () => { live = false; };
  }, [doc.id, doc.revision]);

  useEffect(() => {
    if (!picking) return;
    let live = true;
    const timer = window.setTimeout(() => {
      void workbench("knowledge", WorkbenchRequestSchema, {query}, KnowledgeListSchema)
        .then(value => {
          if (live) setOptions(value.documents.filter(option => option.id !== doc.id));
        })
        .catch(error => { if (live) setError(error instanceof Error ? error.message : "关联失败"); })
        .finally(() => { if (live) setLoaded(true); });
    }, query ? 180 : 0);
    return () => { live = false; window.clearTimeout(timer); };
  }, [picking, query, doc.id]);

  async function toggle(id: string) {
    setBusy(true);
    setError("");
    try {
      const value = await workbench("knowledge-link", KnowledgeLinkRequestSchema, {
        id: doc.id, targetId: id, present: !doc.relatedIds.includes(id), expectedRevision: doc.revision,
      }, KnowledgeDocumentSchema);
      onChanged(value);
    } catch (error) {
      setError(error instanceof Error ? error.message : "关联失败");
    } finally {
      setBusy(false);
    }
  }

  return <section className="knowledge-related" aria-labelledby={`${picker}-label`}>
    <header className="knowledge-related-header">
      <h3 id={`${picker}-label`}>{t("关联文档")}</h3>
      <Button ref={trigger} variant="ghost" size="icon" aria-controls={picker} aria-expanded={picking}
        aria-label={t(picking ? "收起关联选择" : "关联文档")} disabled={disabled || busy}
        onClick={() => {
          if (!picking) setLoaded(false);
          setPicking(!picking);
        }}>{picking ? <X size={17}/> : <Link2 size={17}/>}</Button>
    </header>
    {error && <p role="alert" className="workbench-error">{t(error)}</p>}
    {related.length > 0 && <ul className="knowledge-related-links">
      {related.map(value => <li key={value.id}>
        <button type="button" disabled={disabled || busy} onClick={() => onSelect(value.id)}>
          <span>{value.title}</span><ArrowUpRight size={15} aria-hidden/>
        </button>
      </li>)}
    </ul>}
    {picking && <div id={picker} className="knowledge-link-picker" onKeyDown={event => {
      if (event.key === "Escape") {
        event.preventDefault();
        setPicking(false);
        trigger.current?.focus();
      }
    }}>
      <label><Search size={16} aria-hidden/>
        <input autoFocus aria-label={t("搜索关联文档")} value={query} onChange={event => {
          setLoaded(false);
          setQuery(event.target.value);
        }}
          placeholder={t("搜索文档")}/>
      </label>
      <div className="knowledge-link-options" aria-busy={!loaded}>
        {options.map(value => {
          const linked = doc.relatedIds.includes(value.id);
          return <button type="button" key={value.id} aria-pressed={linked} disabled={busy || disabled}
            onClick={() => void toggle(value.id)}>
            <span>{value.title}</span>{linked ? <Check size={16} aria-hidden/> : <Link2 size={15} aria-hidden/>}
          </button>;
        })}
        {options.length === 0 && (loaded ? <p>{t("没有可关联的文档")}</p> : <LoadingOrb size={18}/>)}
      </div>
    </div>}
  </section>;
}
