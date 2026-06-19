Read Image or video content from a file.
- A `<system>` tag will be given before the read file content.
- The maximum size that can be read is ${MAX_MEDIA_MEGABYTES}MB. An error will be returned if the file is larger than this limit.
**Capabilities**
{% if "image_in" in capabilities and "video_in" in capabilities %}
- This tool supports image and video files for the current model.
{% elif "image_in" in capabilities %}
- This tool supports image files for the current model.
- Video files are not supported by the current model.
{% elif "video_in" in capabilities %}
- This tool supports video files for the current model.
- Image files are not supported by the current model.
{% else %}
- The current model does not support image or video input.
{% endif %}
