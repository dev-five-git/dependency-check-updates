#!/usr/bin/env node
const { main } = require('./index.js')
;(async () => {
  try {
    await main(process.argv.slice(2))
  } catch (err) {
    console.error(err)
    process.exit(1)
  }
})()
