/* GStreamer
 * Copyright (C) <2007> Wim Taymans <wim.taymans@gmail.com>
 *
 * This library is free software; you can redistribute it and/or
 * modify it under the terms of the GNU Library General Public
 * License as published by the Free Software Foundation; either
 * version 2 of the License, or (at your option) any later version.
 *
 * This library is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
 * Library General Public License for more details.
 *
 * You should have received a copy of the GNU Library General Public
 * License along with this library; if not, write to the
 * Free Software Foundation, Inc., 51 Franklin St, Fifth Floor,
 * Boston, MA 02110-1301, USA.
 */

#ifndef __RTP_JITTER_BUFFER_H__
#define __RTP_JITTER_BUFFER_H__

#include <gst/gst.h>
#include <gst/rtp/gstrtcpbuffer.h>

typedef struct _RTPJitterBuffer TsRTPJitterBuffer;
typedef struct _RTPJitterBufferClass TsRTPJitterBufferClass;
typedef struct _RTPJitterBufferItem RTPJitterBufferItem;

#define RTP_TYPE_JITTER_BUFFER             (ts_rtp_jitter_buffer_get_type())
#define RTP_JITTER_BUFFER(src)             (G_TYPE_CHECK_INSTANCE_CAST((src),RTP_TYPE_JITTER_BUFFER,TsRTPJitterBuffer))
#define RTP_JITTER_BUFFER_CLASS(klass)     (G_TYPE_CHECK_CLASS_CAST((klass),RTP_TYPE_JITTER_BUFFER,TsRTPJitterBufferClass))
#define RTP_IS_JITTER_BUFFER(src)          (G_TYPE_CHECK_INSTANCE_TYPE((src),RTP_TYPE_JITTER_BUFFER))
#define RTP_IS_JITTER_BUFFER_CLASS(klass)  (G_TYPE_CHECK_CLASS_TYPE((klass),RTP_TYPE_JITTER_BUFFER))
#define RTP_JITTER_BUFFER_CAST(src)        ((TsRTPJitterBuffer *)(src))

/**
 * RTPJitterBufferMode:
 * @RTP_JITTER_BUFFER_MODE_NONE: don't do any skew correction, outgoing
 *    timestamps are calculated directly from the RTP timestamps. This mode is
 *    good for recording but not for real-time applications.
 * @RTP_JITTER_BUFFER_MODE_SLAVE: calculate the skew between sender and receiver
 *    and produce smoothed adjusted outgoing timestamps. This mode is good for
 *    low latency communications.
 * @RTP_JITTER_BUFFER_MODE_BUFFER: buffer packets between low/high watermarks.
 *    This mode is good for streaming communication.
 * @RTP_JITTER_BUFFER_MODE_SYNCED: sender and receiver clocks are synchronized,
 *    like #RTP_JITTER_BUFFER_MODE_SLAVE but skew is assumed to be 0. Good for
 *    low latency communication when sender and receiver clocks are
 *    synchronized and there is thus no clock skew.
 * @RTP_JITTER_BUFFER_MODE_LAST: last buffer mode.
 *
 * The different buffer modes for a jitterbuffer.
 */
typedef enum {
  RTP_JITTER_BUFFER_MODE_NONE    = 0,
  RTP_JITTER_BUFFER_MODE_SLAVE   = 1,
  RTP_JITTER_BUFFER_MODE_BUFFER  = 2,
  /* FIXME 3 is missing because it was used for 'auto' in jitterbuffer */
  RTP_JITTER_BUFFER_MODE_SYNCED  = 4,
  RTP_JITTER_BUFFER_MODE_LAST
} RTPJitterBufferMode;

#define RTP_TYPE_JITTER_BUFFER_MODE (ts_rtp_jitter_buffer_mode_get_type())
GType ts_rtp_jitter_buffer_mode_get_type (void);

#define RTP_JITTER_BUFFER_MAX_WINDOW 512
/**
 * RTPJitterBuffer:
 *
 * A JitterBuffer in the #RTPSession
 */
struct _RTPJitterBuffer {
  GObject        object;

  GQueue        *packets;

  RTPJitterBufferMode mode;

  GstClockTime   delay;

  /* for buffering */
  gboolean          buffering;
  guint64           low_level;
  guint64           high_level;

  /* for calculating skew */
  gboolean       need_resync;
  GstClockTime   base_time;
  GstClockTime   base_rtptime;
  GstClockTime   media_clock_base_time;
  guint32        clock_rate;
  GstClockTime   base_extrtp;
  GstClockTime   prev_out_time;
  guint64        ext_rtptime;
  guint64        last_rtptime;
  gint64         window[RTP_JITTER_BUFFER_MAX_WINDOW];
  guint          window_pos;
  guint          window_size;
  gboolean       window_filling;
  gint64         window_min;
  gint64         skew;
  gint64         prev_send_diff;
  gboolean       buffering_disabled;

  GMutex         clock_lock;
  GstClock      *pipeline_clock;
  GstClock      *media_clock;
  gulong         media_clock_synced_id;
  guint64        media_clock_offset;

  gboolean       rfc7273_sync;
};

struct _RTPJitterBufferClass {
  GObjectClass   parent_class;
};

/**
 * RTPJitterBufferItem:
 * @data: the data of the item
 * @next: pointer to next item
 * @prev: pointer to previous item
 * @type: the type of @data, used freely by caller
 * @dts: input DTS
 * @pts: output PTS
 * @seqnum: seqnum, the seqnum is used to insert the item in the
 *   right position in the jitterbuffer and detect duplicates. Use -1 to
 *   append.
 * @count: amount of seqnum in this item
 * @rtptime: rtp timestamp
 *
 * An object containing an RTP packet or event.
 */
struct _RTPJitterBufferItem {
  gpointer data;
  GList *next;
  GList *prev;
  guint type;
  GstClockTime dts;
  GstClockTime pts;
  guint seqnum;
  guint count;
  guint rtptime;
};

GType ts_rtp_jitter_buffer_get_type (void);

/* managing lifetime */
TsRTPJitterBuffer*      ts_rtp_jitter_buffer_new              (void);

RTPJitterBufferMode   ts_rtp_jitter_buffer_get_mode         (TsRTPJitterBuffer *jbuf);
void                  ts_rtp_jitter_buffer_set_mode         (TsRTPJitterBuffer *jbuf, RTPJitterBufferMode mode);

GstClockTime          ts_rtp_jitter_buffer_get_delay        (RTPJitterBuffer *jbuf);
void                  ts_rtp_jitter_buffer_set_delay        (TsRTPJitterBuffer *jbuf, GstClockTime delay);

void                  ts_rtp_jitter_buffer_set_clock_rate   (TsRTPJitterBuffer *jbuf, guint32 clock_rate);
guint32               ts_rtp_jitter_buffer_get_clock_rate   (RTPJitterBuffer *jbuf);

void                  ts_rtp_jitter_buffer_set_media_clock  (TsRTPJitterBuffer *jbuf, GstClock * clock, guint64 clock_offset);
void                  ts_rtp_jitter_buffer_set_pipeline_clock (TsRTPJitterBuffer *jbuf, GstClock * clock);

gboolean              ts_rtp_jitter_buffer_get_rfc7273_sync (RTPJitterBuffer *jbuf);
void                  ts_rtp_jitter_buffer_set_rfc7273_sync (TsRTPJitterBuffer *jbuf, gboolean rfc7273_sync);

void                  ts_rtp_jitter_buffer_reset_skew       (TsRTPJitterBuffer *jbuf);

gboolean              ts_rtp_jitter_buffer_insert           (RTPJitterBuffer *jbuf,
                                                             RTPJitterBufferItem *item,
                                                             gboolean *head, gint *percent);

void                  ts_rtp_jitter_buffer_disable_buffering (TsRTPJitterBuffer *jbuf, gboolean disabled);

RTPJitterBufferItem * ts_rtp_jitter_buffer_peek             (TsRTPJitterBuffer *jbuf);
RTPJitterBufferItem * ts_rtp_jitter_buffer_pop              (TsRTPJitterBuffer *jbuf, gint *percent);

void                  ts_rtp_jitter_buffer_flush            (TsRTPJitterBuffer *jbuf,
                                                             GFunc free_func, gpointer user_data);

gboolean              ts_rtp_jitter_buffer_is_buffering     (RTPJitterBuffer * jbuf);
void                  ts_rtp_jitter_buffer_set_buffering    (TsRTPJitterBuffer * jbuf, gboolean buffering);
gint                  ts_rtp_jitter_buffer_get_percent      (RTPJitterBuffer * jbuf);

guint                 ts_rtp_jitter_buffer_num_packets      (RTPJitterBuffer *jbuf);
guint32               ts_rtp_jitter_buffer_get_ts_diff      (RTPJitterBuffer *jbuf);

void                  ts_rtp_jitter_buffer_get_sync         (TsRTPJitterBuffer *jbuf, guint64 *rtptime,
                                                             guint64 *timestamp, guint32 *clock_rate,
                                                             guint64 *last_rtptime);

GstClockTime          ts_rtp_jitter_buffer_calculate_pts    (RTPJitterBuffer * jbuf, GstClockTime dts, gboolean estimated_dts,
                                                             guint32 rtptime, GstClockTime base_time, gint gap,
                                                             gboolean is_rtx);

gboolean              ts_rtp_jitter_buffer_can_fast_start   (RTPJitterBuffer * jbuf, gint num_packet);

gboolean              ts_rtp_jitter_buffer_is_full          (RTPJitterBuffer * jbuf);
void                  ts_rtp_jitter_buffer_find_earliest    (TsRTPJitterBuffer * jbuf, GstClockTime *pts, guint * seqnum);

#endif /* __RTP_JITTER_BUFFER_H__ */
