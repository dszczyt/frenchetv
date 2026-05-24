package com.frenchetv

import android.app.Activity
import android.net.Uri
import android.os.Bundle
import android.view.View
import android.view.WindowManager
import androidx.media3.common.C
import androidx.media3.common.MediaItem
import androidx.media3.common.Player
import androidx.media3.datasource.DefaultHttpDataSource
import androidx.media3.exoplayer.ExoPlayer
import androidx.media3.exoplayer.source.DefaultMediaSourceFactory
import androidx.media3.ui.PlayerView

/**
 * Full-screen ExoPlayer activity launched by the Rust UI via JNI.
 *
 * Extras expected on the launch Intent:
 *   "stream_url"   (String, required) — HLS / DASH stream URL
 *   "auth_header"  (String, optional) — "Bearer <token>" or "Basic ..."
 *   "license_url"  (String, optional) — Widevine license server URL
 *
 * The user exits by pressing the remote's Back button.
 */
class PlayerActivity : Activity() {

    private var player: ExoPlayer? = null
    private lateinit var playerView: PlayerView

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        keepScreenOn()

        val streamUrl = intent.getStringExtra("stream_url") ?: run {
            finish()
            return
        }
        val authHeader = intent.getStringExtra("auth_header")
        val licenseUrl = intent.getStringExtra("license_url")

        // Full-screen player view
        playerView = PlayerView(this)
        playerView.useController = false       // TV remote handles everything
        setContentView(playerView)

        val httpDataSourceFactory = DefaultHttpDataSource.Factory().apply {
            if (authHeader != null) {
                setDefaultRequestProperties(mapOf("Authorization" to authHeader))
            }
        }

        val mediaSourceFactory = DefaultMediaSourceFactory(httpDataSourceFactory)

        val mediaItemBuilder = MediaItem.Builder().setUri(Uri.parse(streamUrl))

        if (licenseUrl != null) {
            mediaItemBuilder.setDrmConfiguration(
                MediaItem.DrmConfiguration.Builder(C.WIDEVINE_UUID)
                    .setLicenseUri(licenseUrl)
                    .build()
            )
        }

        val exoPlayer = ExoPlayer.Builder(this)
            .setMediaSourceFactory(mediaSourceFactory)
            .build()

        exoPlayer.setMediaItem(mediaItemBuilder.build())
        exoPlayer.playWhenReady = true
        exoPlayer.prepare()

        exoPlayer.addListener(object : Player.Listener {
            override fun onPlaybackStateChanged(playbackState: Int) {
                if (playbackState == Player.STATE_ENDED) {
                    finish()
                }
            }
        })

        playerView.player = exoPlayer
        player = exoPlayer
    }

    override fun onResume() {
        super.onResume()
        player?.play()
    }

    override fun onPause() {
        super.onPause()
        player?.pause()
    }

    override fun onDestroy() {
        super.onDestroy()
        player?.release()
        player = null
    }

    // ── Helpers ──────────────────────────────────────────────────────────────

    private fun keepScreenOn() {
        window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        // Immersive full-screen
        @Suppress("DEPRECATION")
        window.decorView.systemUiVisibility = (
            View.SYSTEM_UI_FLAG_FULLSCREEN
                or View.SYSTEM_UI_FLAG_HIDE_NAVIGATION
                or View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY
        )
    }
}
