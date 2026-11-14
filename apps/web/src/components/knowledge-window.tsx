"use client";

import {
  KnowledgeDeleteRequestSchema, KnowledgeDocumentSchema, KnowledgeImageSchema,
  KnowledgeImageUploadRequestSchema, KnowledgeListSchema, KnowledgeSaveRequestSchema,
  WorkbenchRequestSchema,
} from "@ai-phone/protocol/ai_phone/v1/workbench_pb";
import type {MessageShape} from "@bufbuild/protobuf";
import {ImagePlus, PenLine, Plus, Save, Search, Trash2} from "lucide-react";
import {useEffect, useId, useRef, useState} from "react";
import Markdown, {type Components} from "react-markdown";
import remarkGfm from "remark-gfm";
import {workbench} from "@/lib/workbench-client";
import {FloatingWindow, rememberAnchor} from "./floating-window";
import {KnowledgeNetwork} from "./knowledge-network";
import {KnowledgeRelations} from "./knowledge-relations";
import {useI18n} from "./locale-provider";
import {Button} from "./ui/button";
import {LoadingOrb} from "./ui/loading-orb";
import "./knowledge-window.css";

type Doc = MessageShape<typeof KnowledgeDocumentSchema>;
type Draft = Pick<Doc, "id" | "title" | "content" | "revision" | "relatedIds"> & {dirty?: boolean};
const NEW_DOCUMENT = "new";
const IMAGE_TYPES = ["image/png", "image/jpeg", "image/webp"];
const MAX_IMAGE = 6 * 1024 * 1024;

export function KnowledgeWindow({open, onClose}: {open: boolean; onClose: () => void}) {
  const {t} = useI18n();
  const inputId = useId();
  const [docs, setDocs] = useState<Doc[]>([]);
  const [loaded, setLoaded] = useState(false);
  const [query, setQuery] = useState("");
  const [libraryError, setLibraryError] = useState("");
  const [documentError, setDocumentError] = useState("");
  const [busy, setBusy] = useState(false);
  const [selected, setSelected] = useState("");
  const [anchorKey, setAnchorKey] = useState("");
  const [editorOpen, setEditorOpen] = useState(false);
  const [editing, setEditing] = useState(false);
  const [drafts, setDrafts] = useState<Record<string, Draft>>({});
  const [refresh, setRefresh] = useState(0);
  const selectedRef = useRef("");
  const imagePicker = useRef<HTMLInputElement>(null);
  const contentField = useRef<HTMLTextAreaElement>(null);
  selectedRef.current = selected;
  const draft = drafts[selected];
  const listedRevision = docs.find(doc => doc.id === selected)?.revision;
  const revision = draft?.revision;
  const dirty = !!draft?.dirty;

  useEffect(() => {
    if (!open) return;
    let live = true;
    let loading = false;
    async function load() {
      if (loading) return;
      loading = true;
      try {
        const result = await workbench("knowledge", WorkbenchRequestSchema, {query}, KnowledgeListSchema);
        if (live) {
          setDocs(result.documents);
          setLibraryError("");
        }
      } catch (error) {
        if (live) setLibraryError(error instanceof Error ? error.message : "读取失败");
      } finally {
        if (live) setLoaded(true);
        loading = false;
      }
    }
    const timer = window.setTimeout(() => void load(), query ? 180 : 0);
    const interval = window.setInterval(() => {
      if (!document.hidden) void load();
    }, 4000);
    return () => {
      live = false;
      window.clearTimeout(timer);
      window.clearInterval(interval);
    };
  }, [open, query, refresh]);

  useEffect(() => {
    if (!open || !editorOpen || !selected || selected === NEW_DOCUMENT || busy) return;
    if (editing && dirty) return;
    if (revision !== undefined && (listedRevision === undefined || listedRevision <= revision)) return;
    let live = true;
    void workbench("knowledge-read", WorkbenchRequestSchema, {id: selected}, KnowledgeDocumentSchema)
      .then(value => {
        if (live) setDrafts(current => {
          const prior = current[value.id];
          return prior?.dirty || (prior && prior.revision >= value.revision) ? current : {...current, [value.id]: value};
        });
      })
      .catch(error => {
        if (live) setDocumentError(error instanceof Error ? error.message : "读取失败");
      });
    return () => { live = false; };
  }, [open, editorOpen, selected, listedRevision, revision, editing, busy, dirty]);

  function select(id: string, fromGraph = false) {
    if (fromGraph) setAnchorKey(id);
    if (selected === id && editorOpen) return;
    setSelected(id);
    setDocumentError("");
    setEditorOpen(true);
    setEditing(!!drafts[id]?.dirty);
  }

  function startDocument() {
    if (busy) return;
    if (!drafts[NEW_DOCUMENT]) {
      setDrafts(current => ({
        ...current,
        [NEW_DOCUMENT]: {id: "", title: "", content: "", revision: 0n, relatedIds: []},
      }));
    }
    setAnchorKey(NEW_DOCUMENT);
    setSelected(NEW_DOCUMENT);
    setEditing(true);
    setEditorOpen(true);
    setDocumentError("");
  }

  function change(changes: Pick<Partial<Draft>, "title" | "content">) {
    setDrafts(current => ({...current, [selected]: {...current[selected], ...changes, dirty: true}}));
  }

  function accept(value: Doc) {
    setDrafts(current => ({...current, [value.id]: value}));
    setRefresh(count => count + 1);
  }

  async function insertImage(file: File) {
    const field = contentField.current;
    if (!draft || !field || busy) return;
    if (!IMAGE_TYPES.includes(file.type) || file.size > MAX_IMAGE) {
      setDocumentError("请使用 6 MB 以内的 PNG、JPEG 或 WebP 图片");
      return;
    }
    const key = selected;
    const start = field.selectionStart;
    const end = field.selectionEnd;
    setBusy(true);
    setDocumentError("");
    try {
      const image = await workbench("knowledge-image-upload", KnowledgeImageUploadRequestSchema, {
        image: new Uint8Array(await file.arrayBuffer()),
      }, KnowledgeImageSchema);
      const title = file.name.replace(/\.[^.]+$/, "").replace(/[\[\]\\\r\n]/g, " ").trim() || t("图片");
      const markdown = `\n\n![${title}](/api/workbench/knowledge-image/${image.id})\n\n`;
      setDrafts(current => {
        const prior = current[key];
        return {...current, [key]: {
          ...prior, dirty: true, content: prior.content.slice(0, start) + markdown + prior.content.slice(end),
        }};
      });
    } catch (error) {
      setDocumentError(error instanceof Error ? error.message : "图片上传失败");
    } finally {
      setBusy(false);
    }
  }

  const latestChanged = editing && dirty && revision !== undefined && listedRevision !== undefined && listedRevision > revision;
  const canSave = !!draft?.title.trim() && (!draft.id || !!draft.dirty) && !busy;
  async function save() {
    if (!draft || !canSave) return;
    const key = selected;
    setBusy(true);
    setDocumentError("");
    try {
      const value = await workbench("knowledge-save", KnowledgeSaveRequestSchema, {
        id: draft.id, title: draft.title, content: draft.content,
        expectedRevision: draft.revision, relatedIds: draft.relatedIds,
      }, KnowledgeDocumentSchema);
      if (!draft.id) {
        setDrafts(current => {
          const next = {...current};
          delete next[NEW_DOCUMENT];
          return next;
        });
      }
      accept(value);
      if (selectedRef.current === key) {
        setSelected(value.id);
        setEditing(false);
      }
    } catch (error) {
      setDocumentError(error instanceof Error ? error.message : "保存失败");
    } finally {
      setBusy(false);
    }
  }

  async function remove() {
    if (!draft?.id || busy) return;
    setBusy(true);
    setDocumentError("");
    try {
      await workbench("knowledge-delete", KnowledgeDeleteRequestSchema, {
        id: draft.id, expectedRevision: draft.revision,
      }, KnowledgeListSchema);
      if (selectedRef.current === draft.id) setEditorOpen(false);
      setRefresh(count => count + 1);
    } catch (error) {
      setDocumentError(error instanceof Error ? error.message : "移除失败");
    } finally {
      setBusy(false);
    }
  }

  function reloadDocument() {
    const key = selected;
    setDrafts(current => {
      const next = {...current};
      delete next[key];
      return next;
    });
    setDocumentError("");
    setEditing(false);
  }

  return <>
    <FloatingWindow id="knowledge" title={t("知识库")} open={open} onClose={onClose} width={850} height={590}
      actions={<Button variant="ghost" size="icon" aria-label={t("新建知识文档")} disabled={busy} onClick={event => {
        rememberAnchor(event.currentTarget, event.detail === 0 ? "keyboard" : "click");
        startDocument();
      }}><Plus size={18}/></Button>}>
      <div className="knowledge-search">
        <Search size={17} aria-hidden/>
        <input aria-label={t("搜索知识库")} placeholder={t("搜索")} value={query} onChange={event => setQuery(event.target.value)}/>
      </div>
      {libraryError && <p className="workbench-error" role="alert">{t(libraryError)}</p>}
      <div className="knowledge-view" aria-busy={!loaded}>
        {!loaded ? <div className="knowledge-empty"><LoadingOrb size={22}/></div> : docs.length === 0 ? <div className="knowledge-empty">
          <p>{t(query ? "没有匹配的文档" : "知识库为空")}</p>
        </div> : <KnowledgeNetwork docs={docs} selected={selected} onSelect={doc => select(doc.id, true)}/>}
      </div>
    </FloatingWindow>

    <FloatingWindow id="knowledge-document" parentId="knowledge" anchorKey={anchorKey}
      title={draft?.title || docs.find(doc => doc.id === selected)?.title || t("新建文档")}
      open={open && editorOpen} onClose={() => setEditorOpen(false)} width={620} height={600}
      actions={draft && <>
        {editing ? <>
          <Button variant="ghost" size="icon" title={t("插入图片")} aria-label={t("插入图片")}
            disabled={busy} onClick={() => imagePicker.current?.click()}><ImagePlus size={17}/></Button>
          <Button variant="ghost" size="icon" aria-label={t("保存文档")} disabled={!canSave}
            onClick={() => void save()}><Save size={17}/></Button>
        </> : <Button variant="ghost" size="icon" aria-label={t("编辑文档")}
          onClick={() => setEditing(true)}><PenLine size={17}/></Button>}
        {draft.id && <Button variant="ghost" size="icon" aria-label={t("移除当前文档")} disabled={busy}
          onClick={() => void remove()}><Trash2 size={16}/></Button>}
      </>}>
      <div className="knowledge-document" aria-busy={busy || (!draft && !documentError)}>
        {documentError && <p className="workbench-error" role="alert">{t(documentError)}</p>}
        {latestChanged && <div className="knowledge-conflict">
          <p role="status">{t("文档已更新，当前草稿已保留")}</p>
          <Button variant="ghost" disabled={busy} onClick={reloadDocument}>{t("放弃草稿")}</Button>
        </div>}
        {!draft && !documentError && <div className="knowledge-document-loading"><LoadingOrb size={23}/></div>}
        {draft && (editing ? <>
          <input ref={imagePicker} type="file" accept={IMAGE_TYPES.join(",")} hidden onChange={event => {
            const file = event.currentTarget.files?.[0];
            if (file) void insertImage(file);
            event.currentTarget.value = "";
          }}/>
          <div className="knowledge-field">
            <label htmlFor={`${inputId}-title`}>{t("标题")}</label>
            <input id={`${inputId}-title`} disabled={busy} className="knowledge-title-input"
              value={draft.title} onChange={event => change({title: event.target.value})}/>
          </div>
          <div className="knowledge-field knowledge-body-field">
            <label htmlFor={`${inputId}-content`}>{t("正文")}</label>
            <textarea id={`${inputId}-content`} ref={contentField} disabled={busy} className="knowledge-content-input"
              value={draft.content} onChange={event => change({content: event.target.value})}
            onPaste={event => {
              const image = Array.from(event.clipboardData.files).find(file => file.type.startsWith("image/"));
              if (image) {
                event.preventDefault();
                void insertImage(image);
              }
            }}
            onKeyDown={event => {
              if (event.key === "Enter" && (event.metaKey || event.ctrlKey) && !event.nativeEvent.isComposing) {
                event.preventDefault();
                void save();
              }
            }}/>
          </div>
        </> : <>
          <div className="markdown-document"><Markdown remarkPlugins={[remarkGfm]} components={markdownComponents}>{draft.content}</Markdown></div>
          {draft.id && <KnowledgeRelations key={draft.id} doc={draft} disabled={busy}
            onSelect={select} onChanged={accept}/>}
        </>)}
      </div>
    </FloatingWindow>
  </>;
}

const markdownComponents: Components = {
  img: ({alt, src}) => typeof src === "string" && src.startsWith("/api/workbench/knowledge-image/")
    ? <img src={src} alt={alt ?? ""} loading="lazy" decoding="async"/>
    : null,
};
