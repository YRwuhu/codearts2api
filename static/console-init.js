// Classes in the vendor scripts are global lexical bindings. Expose them to app.js.
if (typeof MxSmoothCorner !== 'undefined') window.MxSmoothCorner = MxSmoothCorner;
if (typeof MxRipple !== 'undefined') window.MxRipple = MxRipple;
if (typeof MxCounter !== 'undefined') window.MxCounter = MxCounter;
if (typeof MxTheme !== 'undefined') window.MxTheme = MxTheme;
if (typeof MxModal !== 'undefined') window.MxModal = MxModal;
if (typeof MxToast !== 'undefined') window.MxToast = MxToast;
try { window.MxRipple?.init?.(); } catch (_) {}
try { window.MxSmoothCorner?.init?.(); } catch (_) {}
