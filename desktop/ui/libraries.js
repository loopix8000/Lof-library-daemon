const libraryManager = (() => {
  let libraries = [];
  let editing = null;
  let deleting = null;
  let saving = false;
  let refreshGeneration = 0;
  const dialogs = ["library-name-modal", "library-policy-modal", "library-delete-modal"];

  function policyFields() {
    $("history-days").disabled = saving || !$("history-retention-enabled").checked;
    $("history-expire-protected").disabled = saving || !$("history-retention-enabled").checked;
    $("history-recent").disabled = saving || !$("history-smart").checked;
    $("history-bucket").disabled = saving || !$("history-smart").checked;
  }

  function deleteButton() {
    $("library-delete-save").disabled = saving || !deleting || $("library-delete-name").value !== deleting.name;
  }

  function setSaving(value) {
    saving = value;
    for (const id of dialogs) {
      for (const control of $(id).querySelectorAll("input, button")) control.disabled = value;
    }
    policyFields();
    deleteButton();
  }

  async function save(action) {
    if (saving) return;
    setSaving(true);
    try {
      await action();
    } catch (error) {
      failure(error);
    } finally {
      setSaving(false);
      await refresh();
    }
  }

  function render(running) {
    $("library-add").disabled = !running || saving;
    $("libraries-stopped").hidden = running;
    $("libraries-empty").hidden = !running || libraries.length > 0;
    $("libraries").replaceChildren(...libraries.map((library) => {
      const row = element("li");
      const text = element("div", "grow");
      const counts = library.counts;
      const policy = library.history;
      const created = new Date(library.created_at * 1000).toLocaleString("ko-KR");
      text.append(
        element("span", "title", library.name),
        element("span", "meta", `문서 ${counts.documents}개 · 첨부 ${counts.assets}개 · 이력 ${counts.histories}개 · 미해결 충돌 ${counts.conflicts}개`),
        element("span", "meta", `자동 기록 ${policy.min_minutes}분·${policy.min_changed_chars}자 이상 · ${policy.retention_days === null ? "기간 제한 없음" : `${policy.retention_days}일 보관`}${policy.smart.enabled ? " · 스마트 정리 사용" : ""}`),
        element("span", "meta", `생성 ${created}`),
      );
      const actions = element("div", "actions");
      actions.append(
        button("이름 변경", "small", () => openName(library)),
        button("이력 설정", "small", () => openPolicy(library)),
        button("삭제", "small danger-outline", () => openDelete(library.id).catch(failure)),
      );
      for (const control of actions.children) control.disabled = saving;
      row.append(text, actions);
      return row;
    }));
  }

  async function refreshList(running) {
    const generation = ++refreshGeneration;
    const result = running ? (await invoke("libraries_list")).libraries : [];
    if (generation !== refreshGeneration) return;
    libraries = result;
    render(running);
  }

  function openName(library = null) {
    if (saving) return;
    editing = library;
    $("library-name-title").textContent = library ? "서재 이름 변경" : "서재 만들기";
    $("library-name-save").textContent = library ? "저장" : "만들기";
    $("library-name").value = library?.name || "";
    openModal("library-name-modal");
  }

  function openPolicy(library) {
    if (saving) return;
    editing = library;
    const policy = library.history;
    $("library-policy-name").textContent = library.name;
    $("history-minutes").value = policy.min_minutes;
    $("history-chars").value = policy.min_changed_chars;
    $("history-retention-enabled").checked = policy.retention_days !== null;
    $("history-days").value = policy.retention_days ?? 30;
    $("history-expire-protected").checked = policy.expire_protected;
    $("history-smart").checked = policy.smart.enabled;
    $("history-recent").value = policy.smart.keep_recent_minutes;
    $("history-bucket").value = policy.smart.bucket_minutes;
    policyFields();
    openModal("library-policy-modal");
  }

  async function openDelete(id) {
    if (saving) return;
    setSaving(true);
    try {
      const list = (await invoke("libraries_list")).libraries;
      const library = list.find((item) => item.id === id);
      if (!library) throw "이 서재는 이미 삭제되었습니다. 목록을 새로 확인해 주세요.";
      deleting = library;
      const counts = library.counts;
      $("library-delete-summary").textContent = `‘${library.name}’\n문서 ${counts.documents}개, 삭제된 문서 ${counts.deleted_documents}개\n첨부 ${counts.assets}개, 이력 ${counts.histories}개, 충돌 사본 ${counts.conflicts_total}개`;
      $("library-delete-name").value = "";
      openModal("library-delete-modal");
    } finally {
      setSaving(false);
    }
  }

  function init() {
    $("library-add").addEventListener("click", () => openName());
    $("history-retention-enabled").addEventListener("change", policyFields);
    $("history-smart").addEventListener("change", policyFields);
    $("library-delete-name").addEventListener("input", deleteButton);
    for (const control of document.querySelectorAll("[data-library-close]")) {
      control.addEventListener("click", () => {
        if (!saving) closeModal(control.dataset.libraryClose);
      });
    }
    $("library-name-form").addEventListener("submit", (event) => {
      event.preventDefault();
      const library = editing;
      const name = $("library-name").value.trim();
      save(async () => {
        if (library) {
          await invoke("library_rename", { id: library.id, name, expectedVersion: library.settings_version });
        } else {
          await invoke("library_create", { name });
        }
        closeModal("library-name-modal");
        toast(library ? "서재 이름을 바꿨습니다." : "서재를 만들었습니다.");
      });
    });
    $("library-policy-form").addEventListener("submit", (event) => {
      event.preventDefault();
      const library = editing;
      const history = {
        min_minutes: Number($("history-minutes").value),
        min_changed_chars: Number($("history-chars").value),
        retention_days: $("history-retention-enabled").checked ? Number($("history-days").value) : null,
        expire_protected: $("history-retention-enabled").checked && $("history-expire-protected").checked,
        smart: {
          enabled: $("history-smart").checked,
          keep_recent_minutes: Number($("history-recent").value),
          bucket_minutes: Number($("history-bucket").value),
        },
      };
      save(async () => {
        const before = library.history;
        const removesMore = (history.retention_days !== null &&
          (before.retention_days === null || history.retention_days < before.retention_days ||
            (history.expire_protected && !before.expire_protected))) ||
          (history.smart.enabled && (!before.smart.enabled ||
            history.smart.keep_recent_minutes < before.smart.keep_recent_minutes ||
            history.smart.bucket_minutes !== before.smart.bucket_minutes));
        if (removesMore) {
          const ok = await confirmDialog("이력 정리 규칙을 적용할까요?", "보관 기간과 스마트 정리 규칙에 해당하는 과거 이력이 자동 삭제될 수 있습니다. 삭제된 이력은 되돌릴 수 없습니다.", "규칙 적용");
          if (!ok) return;
        }
        await invoke("library_policy", { id: library.id, request: { expected_version: library.settings_version, history } });
        closeModal("library-policy-modal");
        toast("서재의 이력 설정을 저장했습니다.");
      });
    });
    $("library-delete-form").addEventListener("submit", (event) => {
      event.preventDefault();
      const library = deleting;
      const name = $("library-delete-name").value;
      if (!library || name !== library.name) return;
      save(async () => {
        try {
          await invoke("library_delete", { id: library.id, name, expectedRevision: library.revision });
        } catch (error) {
          // 실패 후 같은 확인 내용을 재사용하지 않도록 이름 입력을 지운다.
          $("library-delete-name").value = "";
          closeModal("library-delete-modal");
          throw error;
        }
        closeModal("library-delete-modal");
        deleting = null;
        toast("서버에서 서재를 삭제했습니다.");
      });
    });
  }

  function closeOnEscape() {
    const id = dialogs.find((id) => !$(id).hidden);
    if (!id) return false;
    if (!saving) closeModal(id);
    return true;
  }

  return { init, refresh: refreshList, closeOnEscape };
})();
