import { createApp } from 'vue'
import App from './App.vue'
import { router } from './router'
import { initLocale } from './i18n'
import { initTheme } from './theme'
import './design/tokens.css'

// Theme and language are decided before the first paint by the inline
// bootstrap in index.html; these calls adopt that decision in Vue and keep
// the document metadata in sync.
initTheme()
initLocale()

const app = createApp(App).use(router)

// Wait for the first navigation so a deep link renders the right page instead
// of flashing the status route on the way to another one.
void router.isReady().then(() => app.mount('#app'))
