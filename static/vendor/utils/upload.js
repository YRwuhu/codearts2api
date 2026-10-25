/**
 * MIUIX CONSOLE — Upload Manager
 */
class MxUpload {
  /**
   * Initialize all .mx-upload elements
   */
  static init() {
    // Click to open file dialog
    document.addEventListener('click', e => {
      const upload = e.target.closest('.mx-upload');
      if (!upload || upload.classList.contains('disabled')) return;
      if (e.target.closest('.mx-upload-file-remove')) return;

      const input = upload.querySelector('input[type="file"]');
      if (input) input.click();
    });

    // File input change
    document.addEventListener('change', e => {
      if (e.target.type !== 'file') return;
      const upload = e.target.closest('.mx-upload');
      if (!upload) return;

      const files = Array.from(e.target.files);
      MxUpload._addFiles(upload, files);
      e.target.value = ''; // reset for re-select
    });

    // Drag and drop
    document.addEventListener('dragover', e => {
      const upload = e.target.closest('.mx-upload');
      if (!upload) return;
      e.preventDefault();
      upload.classList.add('dragover');
    });

    document.addEventListener('dragleave', e => {
      const upload = e.target.closest('.mx-upload');
      if (!upload) return;
      upload.classList.remove('dragover');
    });

    document.addEventListener('drop', e => {
      const upload = e.target.closest('.mx-upload');
      if (!upload) return;
      e.preventDefault();
      upload.classList.remove('dragover');

      const files = Array.from(e.dataTransfer.files);
      MxUpload._addFiles(upload, files);
    });

    // Remove file
    document.addEventListener('click', e => {
      const removeBtn = e.target.closest('.mx-upload-file-remove');
      if (!removeBtn) return;
      const fileEl = removeBtn.closest('.mx-upload-file');
      const upload = removeBtn.closest('.mx-upload');
      if (fileEl) {
        fileEl.style.opacity = '0';
        fileEl.style.transform = 'translateX(10px)';
        setTimeout(() => fileEl.remove(), 200);
      }
      if (upload) {
        upload.dispatchEvent(new CustomEvent('mx-upload-remove', {
          detail: { index: MxUpload._getFileIndex(fileEl) }
        }));
      }
    });
  }

  static _addFiles(upload, files) {
    let list = upload.querySelector('.mx-upload-files');
    if (!list) {
      list = document.createElement('div');
      list.className = 'mx-upload-files';
      upload.appendChild(list);
    }

    const accept = upload.querySelector('input[type="file"]')?.accept;

    files.forEach(file => {
      // Check accept
      if (accept && !MxUpload._matchesAccept(file.name, file.type, accept)) return;

      const icon = MxUpload._getFileIcon(file.name);
      const size = MxUpload._formatSize(file.size);
      const el = document.createElement('div');
      el.className = 'mx-upload-file';
      el.innerHTML = `
        <div class="mx-upload-file-icon">${icon}</div>
        <div class="mx-upload-file-info">
          <div class="mx-upload-file-name">${file.name}</div>
          <div class="mx-upload-file-size">${size}</div>
        </div>
        <span class="mx-upload-file-status success">就绪</span>
        <button class="mx-upload-file-remove">
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><line x1="18" y1="6" x2="6" y2="18"/><line x1="6" y1="6" x2="18" y2="18"/></svg>
        </button>`;

      list.appendChild(el);
      requestAnimationFrame(() => {
        el.style.opacity = '0';
        el.style.transform = 'translateY(4px)';
        requestAnimationFrame(() => {
          el.style.transition = 'all 0.2s ease';
          el.style.opacity = '1';
          el.style.transform = 'translateY(0)';
        });
      });
    });

    upload.dispatchEvent(new CustomEvent('mx-upload-change', {
      detail: { files }
    }));
  }

  static _getFileIcon(name) {
    const ext = name.split('.').pop().toLowerCase();
    const map = {
      pdf: '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"/><polyline points="14 2 14 8 20 8"/></svg>',
      jpg: '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><rect x="3" y="3" width="18" height="18" rx="2"/><circle cx="8.5" cy="8.5" r="1.5"/><polyline points="21 15 16 10 5 21"/></svg>',
      png: '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><rect x="3" y="3" width="18" height="18" rx="2"/><circle cx="8.5" cy="8.5" r="1.5"/><polyline points="21 15 16 10 5 21"/></svg>',
      zip: '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M21 8v13H3V3h12l6 5z"/><path d="M12 3v6h-2v2h2v2h-2v2h2v6"/></svg>',
    };
    return map[ext] || '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"/><polyline points="14 2 14 8 20 8"/></svg>';
  }

  static _formatSize(bytes) {
    if (bytes < 1024) return bytes + ' B';
    if (bytes < 1024 * 1024) return (bytes / 1024).toFixed(1) + ' KB';
    return (bytes / (1024 * 1024)).toFixed(1) + ' MB';
  }

  static _matchesAccept(name, type, accept) {
    const ext = '.' + name.split('.').pop().toLowerCase();
    return accept.split(',').some(a => {
      a = a.trim();
      if (a.startsWith('.')) return ext === a;
      if (a.endsWith('/*')) return type.startsWith(a.replace('/*', '/'));
      return type === a;
    });
  }

  static _getFileIndex(el) {
    return Array.from(el.parentElement.children).indexOf(el);
  }
}

if (typeof module !== 'undefined') module.exports = MxUpload;
