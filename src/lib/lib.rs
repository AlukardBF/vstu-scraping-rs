pub mod database;

use std::{io::Write, path::PathBuf};

use anyhow::{anyhow, Context, Result};
use futures::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use soup::{NodeExt, QueryBuilderExt};

#[derive(Debug)]
pub struct Houseplant {
    pub name: String,
    pub image: String,
    pub attributes: Attributes,
}
#[derive(Debug, Default)]
pub struct Attributes {
    pub temperature: Option<Attribute>,
    pub humidity: Option<Attribute>,
    pub illumination: Option<Attribute>,
    pub watering: Option<Attribute>,
    pub soil: Option<Attribute>,
    pub fertilizer: Option<Attribute>,
    pub transplant: Option<Attribute>,
    pub propagation: Option<Attribute>,
    pub features: Option<Attribute>,
}
#[derive(Debug)]
pub struct Attribute {
    pub parameter: String,
    pub value: String,
}

pub struct Scraper<T: database::Database> {
    client: reqwest::Client,
    concurrent_tasks: usize,
    database: Option<T>,
    image_dir: PathBuf,
}

impl<T> Scraper<T>
where
    T: database::Database,
{
    pub fn new(concurrent_tasks: usize, image_dir: &str, database: Option<T>) -> Self {
        Scraper {
            client: reqwest::Client::new(),
            concurrent_tasks,
            database,
            image_dir: PathBuf::from(image_dir),
        }
    }

    pub async fn scraper(&self) -> Result<Vec<Houseplant>> {
        // Get title page
        let url = "https://komnatnie-rastenija.ru/";
        println!("Парсим сайт: {}", url);

        let response = self.client.get(url).send().await?;
        let html = response.text().await?;
        // Parse categories ('Рубрики')
        println!("[1/3] Парсим категории");
        let soup = soup::Soup::new(&html);
        let urls = soup
            .class("cat-item")
            .find_all()
            .filter_map(|node| node.children().next())
            .filter_map(|node| node.get("href"))
            .collect::<Vec<String>>();

        println!("Найдено {} категорий!", urls.len());
        // println!("[2/4] Для каждой категории парсим ссылки на растения");

        let sty = ProgressStyle::default_bar()
            .template("{msg} {wide_bar:.cyan/blue} {pos}/{len}")
            .progress_chars("##-");
        let pb = ProgressBar::new(urls.len() as u64);
        pb.set_style(sty.clone());
        pb.set_message(&format!("[2/3] Для каждой из {} категорий парсим ссылки на растения", urls.len()));

        // For each category get all plants urls
        let mut plants_url = futures::stream::iter(urls)
            .map(|url| {
                let res = async move { self.parse_category(&url).await };
                pb.inc(1);
                res
            })
            .buffer_unordered(self.concurrent_tasks)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .flatten()
            .flatten()
            .collect::<Vec<String>>();

        pb.finish();

        // Remove duplicates
        plants_url.sort_unstable();
        plants_url.dedup();

        println!("Получено {} ссылок на растения", plants_url.len());

        let pb = ProgressBar::new(plants_url.len() as u64);
        pb.set_style(sty.clone());
        pb.set_message(&format!("[3/3] Парсим {} растений", plants_url.len()));

        // Parse all plants info
        let plants_info = futures::stream::iter(plants_url)
            .map(|url| {
                let res = async move {
                    let opt_plant = self.parse_houseplant(&url).await.ok();
                    if let Some(plant) = opt_plant.as_ref() {
                        if let Some(db) = &self.database {
                            db.insert(plant)
                                .await
                                .expect("Failed to insert info into database");
                        }
                    }
                    opt_plant
                };
                pb.inc(1);
                res
            })
            .buffer_unordered(self.concurrent_tasks)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .flatten()
            .collect::<Vec<Houseplant>>();

        pb.finish();

        println!("Готово!");

        Ok(plants_info)
    }

    fn page_count(&self, html: &str) -> usize {
        let soup = soup::Soup::new(&html);
        if let Some(node) = soup.attr("class", "nav-links").find() {
            let count = node.children().count();
            node.children()
                .nth(count - 3)
                .unwrap()
                .text()
                .parse::<usize>()
                .unwrap()
        } else {
            1
        }
    }

    async fn parse_titles(&self, url: &str) -> Option<Vec<String>> {
        let response = self.client.get(url).send().await.ok()?;
        let html = response.text().await.ok()?;
        let soup = soup::Soup::new(&html);
        let url_list = soup
            .tag("a")
            .attr("itemprop", "url")
            .find_all()
            .map(|a| a.get("href").unwrap())
            .collect::<Vec<String>>();
        Some(url_list)
    }

    async fn parse_category(&self, url: &str) -> Option<Vec<String>> {
        // Get page count
        let response = self.client.get(url).send().await.ok()?;
        let html = response.text().await.ok()?;
        // Get page count
        let page_count = self.page_count(&html);
        // Create urls for all pages
        let pages = (1..=page_count)
            .map(|page| url.to_owned() + "/page/" + page.to_string().as_str())
            .collect::<Vec<String>>();

        // Parse plants urls
        let plants_url = futures::stream::iter(pages)
            .map(|url| async move { self.parse_titles(&url).await })
            .buffer_unordered(self.concurrent_tasks)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .flatten()
            .flatten()
            .collect::<Vec<String>>();

        Some(plants_url)
    }

    async fn parse_houseplant(&self, url: &str) -> Result<Houseplant> {
        let response = self.client.get(url).send().await?;
        let html = response.text().await?;

        let soup = soup::Soup::new(&html);
        // Parse plant name
        let plant_name = soup
            .attr("class", "entry-title")
            .find()
            .expect("Can't find title")
            .text();
        let plant_name = plant_name.split('—').next().unwrap().to_string();

        // Parse image url
        let image_url = soup
            .attr("itemprop", "url image")
            .find()
            .ok_or(anyhow!("image not found"))?
            .get("data-src")
            .expect("Can't parse plant image url");
        let image_filename = self.download_image(&image_url).await?;

        // Parse table
        if let Some(node) = soup
            .tag("td")
            .find_all()
            .find(|node| node.text().to_lowercase().contains("полив"))
        {
            // Parse table's rows
            let body = node.parent().unwrap().parent().unwrap();
            let nodes = body.children().filter(|node| node.name() == "tr");
            let list = nodes
                .map(|tr| {
                    let mut children = tr.children();
                    let td1 = children.next().expect("can't find td").text();
                    let td2 = children.next().expect("can't find td").text();
                    Attribute {
                        parameter: td1,
                        value: td2,
                    }
                })
                .collect::<Vec<Attribute>>();
            let attrs = self.parse_attributes(list)?;
            Ok(Houseplant {
                name: plant_name,
                image: image_filename,
                attributes: attrs,
            })
        } else {
            Err(anyhow!("Plant table not found"))
        }
    }

    fn parse_attributes(&self, list: Vec<Attribute>) -> Result<Attributes> {
        lazy_static::lazy_static! {
            static ref RE: regex::Regex = regex::Regex::new(
                concat!(
                    r#"(?P<temp>температ)|"#,
                    r#"(?P<hum>влажн)|"#,
                    r#"(?P<illum>освещен)|"#,
                    r#"(?P<water>полив)|"#,
                    r#"(?P<soil>грунт)|"#,
                    r#"(?P<fertil>подкорм|удобрен)|"#,
                    r#"(?P<trans>пересад)|"#,
                    r#"(?P<prop>размнож)|"#,
                    r#"(?P<feature>особен)"#
                )
            ).unwrap();
        }

        let mut attrs = Attributes::default();
        for item in list {
            let param = item.parameter.to_lowercase();
            let caps: Option<regex::Captures> = RE.captures(&param);
            if caps.is_none() {
                attrs.features = Some(item);
                continue;
            }
            let caps = caps.unwrap();
            if caps.name("temp").is_some() {
                attrs.temperature = Some(item);
            } else if caps.name("hum").is_some() {
                attrs.humidity = Some(item);
            } else if caps.name("illum").is_some() {
                attrs.illumination = Some(item);
            } else if caps.name("water").is_some() {
                attrs.watering = Some(item);
            } else if caps.name("soil").is_some() {
                attrs.soil = Some(item);
            } else if caps.name("fertil").is_some() {
                attrs.fertilizer = Some(item);
            } else if caps.name("trans").is_some() {
                attrs.transplant = Some(item);
            } else if caps.name("prop").is_some() {
                attrs.propagation = Some(item);
            } else if caps.name("feature").is_some() {
                attrs.features = Some(item);
            }
        }
        Ok(attrs)
    }

    async fn download_image(&self, image_url: &str) -> Result<String> {
        // Download image
        let response = self
            .client
            .get(image_url)
            .send()
            .await
            .with_context(|| "Can't get response for image")?;
        let image_bytes = response
            .bytes()
            .await
            .with_context(|| "Can't get bytes from response")?;
        // Save to file
        let current_time = chrono::offset::Local::now();
        let current_millis = current_time.timestamp_millis();
        let image_dir = &self.image_dir;
        let image_filename = current_millis.to_string() + ".jpg";
        let image_path = image_dir.join(&image_filename);
        std::fs::create_dir_all(image_dir).expect("Can't create image dir");
        let mut image_file = std::fs::File::create(image_path).expect("Can't create image file");
        image_file
            .write_all(&image_bytes)
            .expect("Error in writing bytes to image file");
        Ok(image_filename)
    }
}

impl<T> Default for Scraper<T>
where
    T: database::Database,
{
    fn default() -> Self {
        Self {
            client: reqwest::Client::new(),
            concurrent_tasks: 5,
            database: None,
            image_dir: PathBuf::from("./images"),
        }
    }
}

trait OptArg {
    fn get_value(&self) -> Option<&str>;
}

impl OptArg for Option<Attribute> {
    fn get_value(&self) -> Option<&str> {
        self.as_ref()
            .map(|x| Some(x.value.as_str()))
            .unwrap_or(None)
    }
}
