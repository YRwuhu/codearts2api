document.getElementById('loginForm').addEventListener('submit', async (event) => {
  event.preventDefault();
  const button = document.getElementById('submit');
  const error = document.getElementById('error');
  button.disabled = true;
  error.textContent = '';
  try {
    const response = await fetch('/api/access/login', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ password: document.getElementById('password').value }),
    });
    if (!response.ok) {
      const result = await response.json();
      throw new Error(result.error || '登录失败');
    }
    location.reload();
  } catch (e) {
    error.textContent = e.message;
    button.disabled = false;
  }
});
