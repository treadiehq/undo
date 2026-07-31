import tailwindcss from '@tailwindcss/vite'

export default defineNuxtConfig({
  // The app is served by the undo binary from embedded static files; there is
  // no Node server at runtime.
  ssr: false,
  devtools: { enabled: false },
  css: ['~/assets/css/main.css'],
  vite: {
    plugins: [tailwindcss()],
  },
  app: {
    head: {
      title: 'Undo',
      htmlAttrs: { lang: 'en' },
      meta: [
        { name: 'viewport', content: 'width=device-width, initial-scale=1' },
        { name: 'color-scheme', content: 'dark' },
      ],
      link: [{ rel: 'icon', type: 'image/svg+xml', href: '/favicon.ico' }],
    },
  },
  compatibilityDate: '2026-07-01',
})
